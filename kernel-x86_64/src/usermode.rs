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
use kernel_core::frameown::Owner;
use kernel_core::sched::{RoundRobin, TaskId, TaskState};
use kernel_core::syscall::{
    pack_process_info, Syscall, SYS_EMIT, SYS_EXIT, SYS_PROCESS_INFO, SYS_RECV, SYS_REGCHECK,
    SYS_SEND, SYS_YIELD,
};
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
    mov     [rbx + 0], rax           // return value -> saved RAX, restored by resume_frame
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

// ---- isr_ud_entry (#UD, vector 6): an ILLEGAL INSTRUCTION executed in ring 3 -------------------
// ALET-P1-011: the fault CLASSIFIER was already swept exhaustively on the host; what was missing was
// an attack on the ENTRY path itself — a real ring-3 #UD, contained the way the isolation trials are.
// #UD pushes no error code, so the full save-first macro applies and the frame is preserved for the
// dispatcher to read (which is what makes the RIP report meaningful rather than guessed).
.global isr_ud_entry
isr_ud_entry:
    save_frame
    mov     rdi, [rbx + 120]         // faulting RIP, from the frame we just saved
    and     rsp, -16
    call    x86_undefined_opcode
    jmp     resume_return

// ---- isr_gp_entry (#GP, vector 13): a PRIVILEGED instruction executed in ring 3 ----------------
// #GP pushes an ERROR CODE ahead of the iretq frame, so `save_frame`'s offsets do not apply — using
// it here would read the error code as RIP and every field after it would be shifted. Like
// `isr_pf_entry`, this entry saves nothing and unwinds nothing (resume_return restores rsp from
// KERNEL_CTX); it passes the error code, which is the datum that says WHAT was refused.
.global isr_gp_entry
isr_gp_entry:
    mov     rdi, [rsp + 0]           // error code (selector index, or 0 for a non-selector #GP)
    and     rsp, -16
    call    x86_general_protection
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

// Read-only process-info syscall, then EXIT carrying returned rdi.
.global stub_process_info_exit_start
stub_process_info_exit_start:
    mov     eax, 7                   // SYS_PROCESS_INFO
    int     0x80
    mov     rdi, rax                 // SYS_EXIT takes its report value in rdi
    mov     eax, 3                   // SYS_EXIT (rdi = packed counters)
    int     0x80
17: jmp     17b
.global stub_process_info_exit_end
stub_process_info_exit_end:

// ---- adversarial ring-3 entry stubs (ALET-P1-011) ---------------------------------------------
// These do not ask the kernel for anything. They ATTACK the entry paths: one executes an
// instruction that does not exist, the other an instruction ring 3 is not allowed to execute. Each
// must land in its own vector, be classified, and be contained by the supervisor — with the machine
// still running afterwards. Nothing here is a syscall, so nothing here is cooperative.

// An instruction that is architecturally guaranteed to be undefined: #UD, vector 6.
.global stub_ud_start
stub_ud_start:
    ud2
20: jmp     20b
.global stub_ud_end
stub_ud_end:

// `hlt` is privileged (CPL must be 0), so executing it at CPL 3 raises #GP(0). Chosen over `cli`
// because `cli`'s outcome depends on RFLAGS.IOPL, and an invariant that changes meaning with a
// flag the test also sets is not an invariant.
.global stub_gp_start
stub_gp_start:
    hlt
21: jmp     21b
.global stub_gp_end
stub_gp_end:

// ---- register round-trip stub (ALET-P1-009, the `fuzz` half) ----------------------------------
// The static half of ALET-P1-009 pins the TrapFrame's SIZE and OFFSETS with const-asserts. What
// that cannot prove is that the trap ASSEMBLY moves each register to the offset it names: a
// save/restore pair that consistently swaps two registers satisfies every offset assert and is
// still wrong. So: the frame is primed with 15 distinct sentinels, the stub takes a trap
// IMMEDIATELY (touching nothing), the kernel compares all 15 saved slots against the sentinels, and
// the stub then re-presents the register file so the RESTORE direction is proved too.
//
// `int 0x80` first — before any instruction of our own — so what the kernel sees is exactly what
// `resume_frame` loaded. Then a second syscall reports whether every register survived the
// round trip, computed BY THE STUB from its own registers.
.global stub_regfuzz_start
stub_regfuzz_start:
    int     0x80                     // #1: SYS_REGCHECK — kernel inspects the saved file
    int     0x80                     // #2: kernel primed rax/rdi for the verdict call
22: jmp     22b
.global stub_regfuzz_end
stub_regfuzz_end:
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
    static stub_process_info_exit_start: u8;
    static stub_process_info_exit_end: u8;
    static isr_ud_entry: u8;
    static isr_gp_entry: u8;
    static stub_ud_start: u8;
    static stub_ud_end: u8;
    static stub_gp_start: u8;
    static stub_gp_end: u8;
    static stub_regfuzz_start: u8;
    static stub_regfuzz_end: u8;
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

// The assembly above addresses this frame with LITERAL byte offsets (`[rdi + 152]` …), which no
// compiler checks — the manual-ABI hazard ALET-P1-009 names. These asserts are the check: every offset
// the asm uses appears here, so changing a field's type or order fails the BUILD rather than corrupting
// a register file at the next trap. Kept exhaustive on purpose, including the register slots, because a
// partial assert set is what makes the remaining literals look verified when they are not.
const _: () = {
    assert!(core::mem::size_of::<TrapFrame>() == 160);
    assert!(core::mem::align_of::<TrapFrame>() == 8);
    // The register array must start at 0 and be 15 contiguous 8-byte slots: the asm's `[rdi + 0]`
    // through `[rdi + 112]`.
    assert!(core::mem::offset_of!(TrapFrame, regs) == 0);
    assert!(core::mem::size_of::<[u64; 15]>() == 120);
    // The named register indices the Rust side uses must agree with those slots.
    assert!(RAX == 0 && RBX == 1 && RCX == 2 && RDI == 5);
    // Every literal the asm uses for the iretq frame.
    assert!(core::mem::offset_of!(TrapFrame, rip) == 120);
    assert!(core::mem::offset_of!(TrapFrame, cs) == 128);
    assert!(core::mem::offset_of!(TrapFrame, rflags) == 136);
    assert!(core::mem::offset_of!(TrapFrame, rsp) == 144);
    assert!(core::mem::offset_of!(TrapFrame, ss) == 152);
    // …and nothing hides past the last one: the frame is exactly the fields the asm knows about.
    assert!(core::mem::offset_of!(TrapFrame, ss) + 8 == core::mem::size_of::<TrapFrame>());
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

// Capability-secure kernel IPC (gap register Issue 2): send/receive a message body through the
// kernel endpoint, each authorized by the same `CapEngine` (`ipc.send` / `ipc.recv`).
// ALET-P1-009 (`fuzz` half): the trapping task asks the kernel to compare its ENTIRE saved
// register file against the sentinels the frame was primed with. Not a real OS service — a
// deliberate probe of the trap assembly, and the only syscall whose implementation reads the
// `TrapFrame` rather than its two dispatched arguments. (A section note, kept as a plain comment:
// as doc comments they were attributed to the sentinel constant below.)
/// The sentinel a register-fuzz frame carries in `regs[i]`.
///
/// Distinct per index, and none of them a small integer or a plausible address: a bug that zeroes a
/// slot, leaves a stale kernel pointer, or copies the neighbouring register is caught by VALUE, not
/// by luck. `regs[RAX]` is the exception — it must be the syscall number for the trap to dispatch
/// at all — and `SYS_REGCHECK` is distinct from every other sentinel, so the property still holds.
const fn regfuzz_sentinel(i: usize) -> u64 {
    if i == RAX {
        SYS_REGCHECK
    } else {
        0xC0DE_FACE_0000_0000 | ((i as u64) << 8) | (0xA0 | i as u64)
    }
}

/// How many times the register file was checked, and the first slot that did not match (as
/// `index + 1`, so 0 means "nothing mismatched"). Two checks run: one on the way IN (proving the
/// save direction) and one after a full resume (proving the restore direction).
static mut REGFUZZ_CHECKS: u32 = 0;
static mut REGFUZZ_MISMATCH: u32 = 0;
/// Non-register frame fields the second check also requires: CS/SS still name ring 3, RFLAGS still
/// has the reserved bit 1 set, and RSP is still inside the user stack page.
static mut REGFUZZ_FRAME_SANE: bool = false;

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
static mut PROCESS_INFO_RESULT: u64 = u64::MAX;

/// The task supervisor (REQ-REL-002, ADR-042). An UNEXPECTED ring-3 fault used to end the boot; now it
/// terminates that task and the system continues, which is what a supervisor is for.
static mut SUPERVISOR: kernel_core::supervisor::Supervisor =
    kernel_core::supervisor::Supervisor::new();
/// The id the supervisor knows the running excursion by. One excursion runs at a time here, so a counter
/// is enough — a real scheduler would carry the id in the TCB.
static mut CURRENT_TASK: u64 = 0;

/// Read-only view of the supervisor, for the boot invariants.
///
/// SAFETY: single-threaded, IF=0 for the whole user-mode suite; no concurrent access exists.
pub fn supervisor() -> &'static kernel_core::supervisor::Supervisor {
    unsafe { &*addr_of!(SUPERVISOR) }
}

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
    if Syscall::decode(num).is_none() {
        return u64::MAX;
    }
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
        // ALET-P1-009 (`fuzz` half). Reads the SAVED frame, not the two dispatched arguments: the
        // whole question is whether the trap assembly put every register where the const-asserts
        // say it did.
        SYS_REGCHECK => {
            // SAFETY: single-threaded, IF=0. `CURRENT_FRAME` was published by `resume_frame` and
            // written by `save_frame` on the way in, so it points at the live, fully-saved frame.
            unsafe {
                let fp = *addr_of!(CURRENT_FRAME) as *const TrapFrame;
                if fp.is_null() {
                    return u64::MAX;
                }
                let f = &*fp;
                let mut mismatch = 0u32;
                for i in 0..15 {
                    if f.regs[i] != regfuzz_sentinel(i) {
                        mismatch = i as u32 + 1;
                        break;
                    }
                }
                // Report only the FIRST mismatch across both checks: a later clean pass must not
                // erase an earlier failure, or the second check would launder the first.
                if mismatch != 0 && *addr_of!(REGFUZZ_MISMATCH) == 0 {
                    *addr_of_mut!(REGFUZZ_MISMATCH) = mismatch;
                }
                *addr_of_mut!(REGFUZZ_CHECKS) += 1;
                // The non-register half of the frame, checked on every pass: still ring 3, RFLAGS
                // still architecturally valid, RSP still inside the one stack page we mapped. A
                // save that corrupted these would return to ring 0 or to nowhere.
                let sane = (f.cs & 3) == 3
                    && (f.ss & 3) == 3
                    && (f.rflags & (1 << 1)) != 0
                    && f.rsp > USER_STACK_VA
                    && f.rsp <= USER_STACK_TOP;
                *addr_of_mut!(REGFUZZ_FRAME_SANE) = sane;
            }
            // Echo the call number back, and do it deliberately: the return value lands in the
            // task's `rax`, and `rax`'s sentinel IS `SYS_REGCHECK` (see `regfuzz_sentinel`). This
            // syscall exists to be issued TWICE from the same unmodified register file — once on the
            // way in, proving the save direction, and once after a full resume, proving the restore
            // direction — so a conventional `0` here would overwrite the one register the second call
            // needs in order to *be* a second call. It did exactly that: the stub's second
            // `int 0x80` dispatched syscall 0, the check count stuck at 1, and the restore half of
            // ALET-P1-009 was never actually exercised on this target.
            SYS_REGCHECK
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

/// `#UD` dispatch (ALET-P1-011). A ring-3 task executed an instruction that does not exist. There
/// is no capability under which that is a request, so it goes straight to the supervisor — the same
/// policy an undeclared `#PF` gets, reached through a different entry path, which is the point.
#[no_mangle]
pub extern "sysv64" fn x86_undefined_opcode(rip: u64) {
    use kernel_core::faultclass::{classify, kind_name, verdict, Fault};
    // An illegal opcode is not a paging event: nothing was present/absent and nothing was written.
    // It is a USER fault by construction — `isr_ud_entry` runs `save_frame`, which sends any ring-0
    // arrival to `from_ring0_fatal` before this function is reached.
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
    supervise_ring3_fault(kind, verdict(kind), |id, reason| {
        kprintln!(
            "[usermode] ring-3 #UD at rip {:#x} -> {} : task {} TERMINATED ({:?}); system continues",
            rip,
            kind_name(kind),
            id,
            reason
        );
    });
}

/// `#GP` dispatch (ALET-P1-011). A ring-3 task executed a privileged instruction. Same policy, third
/// entry path. The error code is reported because a #GP with a non-zero selector index means
/// something different from #GP(0) — and a handler that discards it cannot tell them apart.
#[no_mangle]
pub extern "sysv64" fn x86_general_protection(error_code: u64) {
    use kernel_core::faultclass::{classify, kind_name, verdict, Fault};
    let f = Fault {
        present: true,
        write: false,
        user: true,
        exec: false,
        reserved_bit: false,
        from_kernel: false,
        unrecognized: None,
    };
    let kind = classify(&f);
    supervise_ring3_fault(kind, verdict(kind), |id, reason| {
        kprintln!(
            "[usermode] ring-3 #GP (error {:#x}) -> {} : task {} TERMINATED ({:?}); system continues",
            error_code,
            kind_name(kind),
            id,
            reason
        );
    });
}

/// The one place a ring-3 fault becomes a supervisor decision, shared by `#PF`, `#UD` and `#GP`.
///
/// Factored out deliberately: three entry paths that each carried their own copy of this policy is
/// three places for the policy to diverge, and a divergence would show up as one vector containing
/// a fault that another escalates — the kind of inconsistency the register exists to prevent.
fn supervise_ring3_fault(
    kind: kernel_core::faultclass::FaultKind,
    verdict: kernel_core::faultclass::FaultVerdict,
    report: impl FnOnce(u64, kernel_core::supervisor::TerminationReason),
) {
    use kernel_core::faultclass::kind_name;
    use kernel_core::sched::TaskId;
    use kernel_core::supervisor::SupervisorAction;
    // SAFETY: single-threaded, IF=0 for the whole user-mode suite; only this path mutates it.
    let (action, id) = unsafe {
        let id = TaskId(*addr_of!(CURRENT_TASK));
        (
            (*addr_of_mut!(SUPERVISOR)).on_fault(Some(id), kind, verdict),
            id,
        )
    };
    match action {
        SupervisorAction::TaskTerminated(reason) => report(id.0, reason),
        SupervisorAction::Escalate(k) => {
            kprintln!("[usermode] ring-3 fault ESCALATED ({})", kind_name(k));
            usermode_fatal(104);
        }
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
            // An UNEXPECTED fault from ring 3. This used to end the boot; now it goes to the supervisor
            // (REQ-REL-002, ADR-042). `isr_pf_entry` reaches here only from ring 3 (a ring-0 trap takes
            // `from_ring0_fatal`), so the fault is a user fault by construction — and the classifier says
            // what KIND, which is what the log needs and what the policy consumes. Returning from here
            // jumps to `resume_return`: the task is abandoned and the scheduler continues. That is the
            // kill-and-continue path, and the suite proves it by taking one on purpose.
            use kernel_core::faultclass::{classify, kind_name, verdict, Fault};
            use kernel_core::sched::TaskId;
            use kernel_core::supervisor::SupervisorAction;
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
            // SAFETY: single-threaded, IF=0; only this dispatcher mutates the supervisor.
            let (action, id) = unsafe {
                let id = TaskId(*addr_of!(CURRENT_TASK));
                (
                    (*addr_of_mut!(SUPERVISOR)).on_fault(Some(id), kind, verdict(kind)),
                    id,
                )
            };
            match action {
                SupervisorAction::TaskTerminated(reason) => {
                    kprintln!(
                        "[usermode] ring-3 #PF at {:#x} -> {} : task {} TERMINATED ({:?}); system continues",
                        fault_va,
                        kind_name(kind),
                        id.0,
                        reason
                    );
                    if let Some(t) = current() {
                        t.fault_va = fault_va;
                    }
                }
                SupervisorAction::Escalate(k) => {
                    kprintln!(
                        "[usermode] ring-3 #PF at {:#x} ESCALATED ({})",
                        fault_va,
                        kind_name(k)
                    );
                    usermode_fatal(104);
                }
            }
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
        frames::free_as(f, Owner::USER);
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
    unsafe {
        *addr_of_mut!(PROCESS_INFO_RESULT) = u64::MAX;
    }

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

/// Run one read-only process-info syscall and carry response through `SYS_EXIT`.
fn run_process_info(grant: bool) -> (u64, bool) {
    let root_main = vm::active_root();
    let root = match vm::build_space() {
        Some(root) => root,
        None => return (u64::MAX, false),
    };
    let code = vm::map_stub_frame(
        root,
        USER_CODE_VA,
        stub_bytes!(stub_process_info_exit_start, stub_process_info_exit_end),
    );
    let stack = vm::map_user(root, USER_STACK_VA, true);
    if code.is_none() || stack.is_none() {
        free_leaf(root, USER_STACK_VA, stack);
        free_leaf(root, USER_CODE_VA, code);
        return (u64::MAX, false);
    }

    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint(
            "ring3-process",
            "process.inspect",
            Scope::All,
            Constraints::none(),
        ));
    }
    set_trial(Trial {
        engine,
        store: Store::new(),
        caps,
        action: "process.inspect",
        armed: false,
        expect_fault_va: 0,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });
    let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    f.regs[RAX] = SYS_PROCESS_INFO;
    f.regs[RDI] = 0;
    unsafe {
        vm::switch_to(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    let result = unsafe { *addr_of!(PROCESS_INFO_RESULT) };
    let allowed = take_trial().allowed;
    free_leaf(root, USER_STACK_VA, stack);
    free_leaf(root, USER_CODE_VA, code);
    (result, allowed)
}

/// Prove hardware isolation: a ring-3 read of a supervisor-only page faults and is contained (not
/// fatal). Returns `(isolation_held, fault_va)`.
/// Run a ring-3 task that reads a supervisor-only page it never declared — an UNEXPECTED fault
/// (REQ-REL-002, ADR-042). Nothing is armed, so the supervisor's kill-and-continue path is what handles
/// it. Returns the task id the supervisor should have terminated.
fn run_unexpected_fault() -> u64 {
    let root_main = vm::active_root();
    let root = match vm::build_space() {
        Some(r) => r,
        None => return 0,
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
        return 0;
    }

    // armed = false: the fault is NOT declared, so `x86_page_fault` must go to the supervisor.
    let t = run_in_space(
        root,
        root_main,
        VA_SUP,
        0,
        CapEngine::new(0xBEEF, 1000),
        Vec::new(),
        false,
        0,
    );
    let _ = t;
    // SAFETY: single-threaded; the id the excursion just used.
    let id = unsafe { *addr_of!(CURRENT_TASK) };

    free_leaf(root, VA_SUP, sup);
    free_leaf(root, USER_STACK_VA, stack);
    free_leaf(root, USER_CODE_VA, code);
    vm::destroy_space(root);
    id
}

/// ALET-P1-011: run one ring-3 task whose *only* instruction is an attack on an entry path, and
/// return the task id the supervisor terminated (0 = the excursion could not be set up).
///
/// `stub` is the adversarial code page. The task holds NO capabilities and asks for nothing — the
/// entire excursion exists to make the CPU take a specific vector from ring 3.
fn run_entry_attack(stub: &[u8]) -> u64 {
    let root_main = vm::active_root();
    let root = match vm::build_space() {
        Some(r) => r,
        None => return 0,
    };
    let code = vm::map_stub_frame(root, USER_CODE_VA, stub);
    let stack = vm::map_user(root, USER_STACK_VA, true);
    if code.is_none() || stack.is_none() {
        free_leaf(root, USER_STACK_VA, stack);
        free_leaf(root, USER_CODE_VA, code);
        return 0;
    }
    // armed = false: nothing about this fault is declared, so the dispatcher must reach the
    // supervisor rather than treat it as an expected isolation proof.
    let _ = run_in_space(
        root,
        root_main,
        0,
        0,
        CapEngine::new(0x11DE, 1000),
        Vec::new(),
        false,
        0,
    );
    // SAFETY: single-threaded; the id the excursion just used.
    let id = unsafe { *addr_of!(CURRENT_TASK) };
    free_leaf(root, USER_STACK_VA, stack);
    free_leaf(root, USER_CODE_VA, code);
    vm::destroy_space(root);
    id
}

/// ALET-P1-009 (`fuzz` half): prime all 15 registers with distinct sentinels, trap, let the kernel
/// compare the saved file, RESUME the same frame, and trap again.
///
/// The second resume is the half that matters. One check proves the SAVE direction; only a task
/// that comes back with the same registers proves the RESTORE direction, and a save/restore pair
/// that swaps two registers consistently would pass every offset const-assert and fail here.
///
/// Returns `(checks_run, first_mismatch_slot, frame_stayed_sane)`.
fn run_register_roundtrip() -> (u32, u32, bool) {
    // SAFETY: single-threaded; reset before the excursion that writes them.
    unsafe {
        *addr_of_mut!(REGFUZZ_CHECKS) = 0;
        *addr_of_mut!(REGFUZZ_MISMATCH) = 0;
        *addr_of_mut!(REGFUZZ_FRAME_SANE) = false;
    }

    let root_main = vm::active_root();
    let root = match vm::build_space() {
        Some(r) => r,
        None => return (0, 0, false),
    };
    let code = vm::map_stub_frame(
        root,
        USER_CODE_VA,
        stub_bytes!(stub_regfuzz_start, stub_regfuzz_end),
    );
    let stack = vm::map_user(root, USER_STACK_VA, true);
    if code.is_none() || stack.is_none() {
        free_leaf(root, USER_STACK_VA, stack);
        free_leaf(root, USER_CODE_VA, code);
        return (0, 0, false);
    }

    set_trial(Trial {
        engine: CapEngine::new(0x0F09, 1000),
        store: Store::new(),
        caps: Vec::new(),
        action: "regcheck",
        armed: false,
        expect_fault_va: 0,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });

    let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    for i in 0..15 {
        f.regs[i] = regfuzz_sentinel(i);
    }

    // SAFETY: single-threaded, IF=0. `root` identity-maps the running kernel exactly as
    // `run_in_space` requires, and the previous root is restored immediately after.
    unsafe {
        vm::switch_to(root);
        // TWO resumes of the SAME frame. `save_frame` writes back into this very struct (it is what
        // `CURRENT_FRAME` points at), so the second resume continues the task at the instruction
        // after its first `int 0x80`, carrying whatever the restore path put in its registers.
        resume_frame(&mut f as *mut TrapFrame);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    let _ = take_trial();

    free_leaf(root, USER_STACK_VA, stack);
    free_leaf(root, USER_CODE_VA, code);
    vm::destroy_space(root);

    // SAFETY: excursion complete; single-threaded read-back.
    unsafe {
        (
            *addr_of!(REGFUZZ_CHECKS),
            *addr_of!(REGFUZZ_MISMATCH),
            *addr_of!(REGFUZZ_FRAME_SANE),
        )
    }
}

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
    // A distinct id per excursion, so the supervisor's records name the task that actually faulted.
    // SAFETY: single-threaded; IF=0 for the suite.
    unsafe { *addr_of_mut!(CURRENT_TASK) += 1 };
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
    // A distinct id per excursion, so the supervisor's records name the task that actually faulted.
    // SAFETY: single-threaded; IF=0 for the suite.
    unsafe { *addr_of_mut!(CURRENT_TASK) += 1 };
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

/// Run two **real ring-3 tasks** — own address spaces, own trap frames, real `iretq` context
/// switches — admitted through the machine's **resident risk advisor** and dispatched by the shared
/// `PriorityScheduler` (REQ-ML-003, ADR-056).
///
/// The x86-64 counterpart of the aarch64 and RISC-V scenarios. It landed last, and deliberately so:
/// this target's ring-3 gate was red on the `trapframe` defect (the `SYS_REGCHECK` return value
/// overwriting the one register the second check needed in order to *be* a second check), and wiring
/// a model into a target whose user-mode gate cannot pass would have proved nothing about either.
///
/// The mechanism is untouched — the same `resume_frame` + CR3 switch [`run_scheduler`] exercises.
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
    let magics: [u64; NTASK] = [0xC3C3, 0xD4D4];
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
    // SAFETY: single-threaded; init the TCBs before any resume.
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

    let advices_before = resident::stats().map(|s| s.advices).unwrap_or(0);

    // Admission: each task is described with what it ACTUALLY holds on this machine — one code page
    // and one stack page — rather than with a plausible-looking constant.
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
        resident::admit(
            &mut policy,
            TaskId(i as u64),
            Priority(5),
            now_secs,
            &submission,
        );
    }

    let mut order: Vec<(usize, u64)> = Vec::new();
    while let Some(TaskId(id)) = policy.schedule_next() {
        let slot = id as usize;
        sched_report(0, false);
        // SAFETY: identical to `run_scheduler` — switch into the task's space, resume until it
        // yields or exits, restore the scheduler's space.
        unsafe {
            vm::switch_to(roots[slot]);
            resume_frame(&mut (*addr_of_mut!(TCBS))[slot].frame as *mut TrapFrame);
            vm::switch_to(root_main);
        }
        resident::observe_schedule();
        let (mag, exited) = unsafe {
            let s = &*addr_of!(SCHED);
            (s.last_magic, s.exited)
        };
        order.push((slot, mag));
        if exited {
            // SAFETY: single-threaded write of run state.
            unsafe { (*addr_of_mut!(TCBS))[slot].done = true };
            policy.finish(TaskId(id));
            resident::observe_outcome(JobId(1), UserId(0), Outcome::Finished);
        }
        if order.len() > 4 * NTASK {
            break;
        }
    }

    cleanup_tasks(&roots, &mut code, &mut stack);

    // The interleaving is deliberately NOT asserted: advice may reorder equals, and demanding a fixed
    // order would be asserting that the advisor had no effect. That every task gets every slice and
    // exits IS asserted — nothing invented, dropped or starved.
    let slices = [
        order.iter().filter(|(s, _)| *s == 0).count(),
        order.iter().filter(|(s, _)| *s == 1).count(),
    ];
    let both_done = unsafe {
        let t = &*addr_of!(TCBS);
        t[0].done && t[1].done
    };
    let ran_ok = both_done && order.len() == 8 && slices[0] == 4 && slices[1] == 4;
    let magic_ok = order.iter().all(|(slot, mag)| *mag == magics[*slot]);
    let both_advised = resident::stats()
        .map(|s| s.advices == advices_before + NTASK as u64)
        .unwrap_or(false);
    (ran_ok, magic_ok, both_advised)
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
        // Start the slice budget when the TASK starts. The PIT free-runs at 100 Hz, so without this
        // a task resumes into whatever is LEFT of the current 10 ms period — which on a loaded host
        // can be nothing, and it would then take IRQ0 before executing a single `inc` and fail the
        // progress invariant for a reason that has nothing to do with state preservation.
        crate::pit::rearm();
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
        // What this invariant is FOR is state preservation: the counter a task built up in `rbx`
        // must still be there when the timer takes the CPU away and gives it back. Going BACKWARDS
        // is the failure — it means a resume handed the task a frame that was not its own.
        //
        // Requiring a STRICT advance every slice asserted something else as well: that the task got
        // enough CPU to execute at least one `inc` before IRQ0 arrived. That is a statement about
        // the HOST, not about Aletheia. `pit::rearm()` above already removed most of it by starting
        // the slice budget when the task starts rather than letting it inherit whatever was left of
        // a free-running 10 ms period — but a TCG vCPU thread can still be descheduled by the host
        // between `rearm` and the `iret` into ring 3, and the period then elapses in wall-clock with
        // the guest not running. Observed: this leg passed, failed, then passed twice on identical
        // code, with the difference being what else the workstation was doing.
        //
        // So the per-slice check is the one that cannot be flaky and cannot be vacuous — never
        // backwards — and the "it really did run" half is asserted ONCE at the end, over the whole
        // run, where a slice that got no CPU is absorbed by the slices that did.
        if seen[slot] && progress < last_progress[slot] {
            progress_ok = false; // state was LOST across the involuntary switch
        }
        seen[slot] = true;
        last_progress[slot] = progress;
        counts[slot] += 1;
        cur = (cur + 1) % NTASK;
    }

    cleanup_tasks(&roots, &mut code, &mut stack);

    let fair = clean && counts.iter().all(|&c| c > 0);
    // The other half of the progress claim, asserted over the whole run rather than per slice: every
    // task must have got somewhere. A task whose counter is still 0 after six slices either never
    // ran or never kept anything, and both of those are the failure this invariant is named for.
    let advanced = last_progress.iter().all(|&p| p > 0);
    (fair, progress_ok && advanced && clean)
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

    // The resident risk advisor is consulted about REAL ring-3 tasks, not only about the
    // commissioning workload (REQ-ML-003, ADR-056). Same address spaces, same trap frames, same
    // `resume_frame` + CR3 switch as the round-robin run above.
    let (advised_ran, advised_magic, both_advised) = run_advised_scheduler();
    check!(
        advised_ran,
        "ring3: two REAL ring-3 tasks admitted through the resident advisor each get every slice and exit"
    );
    check!(
        advised_magic,
        "ring3: the full register file survives each context switch under the advised scheduler too"
    );
    check!(
        both_advised,
        "ring3: the advisor was consulted once per real user-mode task — a live spawn reaches the model"
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

    // The supervisor's POLICY, asserted on every target so `conformance.sh` can require it (REQ-REL-002):
    // a user fault terminates that task, a kernel fault escalates. Checked on a scratch supervisor so the
    // live one's counters stay meaningful for the end-to-end proof immediately below.
    {
        use kernel_core::faultclass::{classify, from_x86_error_code, verdict};
        use kernel_core::sched::TaskId;
        use kernel_core::supervisor::{Supervisor, SupervisorAction, TerminationReason};
        let mut probe = Supervisor::new();
        let user = from_x86_error_code(0b100);
        let ukind = classify(&user);
        let contained = probe.on_fault(Some(TaskId(1)), ukind, verdict(ukind));
        let kernelf = from_x86_error_code(0b011);
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
    }

    // Kill the task, keep the system (REQ-REL-002, ADR-042). A ring-3 task faults at an address it never
    // declared: the supervisor must terminate THAT task, the boot must continue past it, and a later task
    // must still run — which is the difference between detecting a bad access and surviving one.
    {
        let before = supervisor().terminated();
        let dead = run_unexpected_fault();
        check!(
            dead != 0 && supervisor().terminated() == before + 1,
            "supervisor: an undeclared ring-3 fault terminates exactly one task (the boot continues past it)"
        );
        check!(
            !supervisor().may_run(kernel_core::sched::TaskId(dead))
                && matches!(
                    supervisor().reason(kernel_core::sched::TaskId(dead)),
                    Some(kernel_core::supervisor::TerminationReason::Fault(_))
                ),
            "supervisor: the dead task may never run again, and the recorded reason is the fault"
        );
        check!(
            supervisor().escalations() == 0,
            "supervisor: a USER fault was contained, not escalated (kernel bugs stay fatal)"
        );
        // And the system really is still usable: another ring-3 excursion runs to completion afterwards.
        let (held, _va) = run_isolation();
        check!(
            held,
            "supervisor: a later ring-3 task still runs and proves its own invariant after the kill"
        );
    }

    // ---- ALET-P1-009 (`fuzz` half): the trap assembly really does move every register ----------
    // The const-assert block pins the frame's LAYOUT. This pins the trap path's BEHAVIOR over that
    // layout, which no static assertion can: a save/restore pair that swapped two registers
    // consistently would satisfy every offset assert and still corrupt every task it interrupts.
    {
        let (checks, mismatch, frame_sane) = run_register_roundtrip();
        check!(
            checks == 2,
            "trapframe: the task trapped, was resumed, and trapped AGAIN — save and restore both ran"
        );
        check!(
            mismatch == 0,
            "trapframe: all 15 ring-3 registers arrive in the slots the const-asserts name, and survive the resume"
        );
        check!(
            frame_sane,
            "trapframe: the saved CS/SS still name ring 3, RFLAGS stays architecturally valid, RSP stays in the user stack"
        );
    }

    // ---- ALET-P1-011: the ENTRY paths themselves, attacked from ring 3 -------------------------
    // `kernel_core::faultclass` is already swept exhaustively on the host — every x86 error code,
    // every EC/DFSC pair, every `scause`. What that proves is that the CLASSIFIER is fail-closed.
    // It says nothing about whether a real illegal opcode or a real privileged instruction executed
    // at CPL 3 reaches a handler at all, arrives with the right privilege reading, and is contained
    // rather than escalated. Two vectors that were fatal catch-alls until this point now take a
    // deliberate hit each, and are handed straight back afterwards.
    {
        // SAFETY: single-core, IF=0 for the whole suite; the entries are the asm stubs above.
        unsafe {
            idt::install_ring3_fault_traps(
                addr_of!(isr_ud_entry) as u64,
                addr_of!(isr_gp_entry) as u64,
            );
        }

        let before = supervisor().terminated();
        let ud_task = run_entry_attack(stub_bytes!(stub_ud_start, stub_ud_end));
        check!(
            ud_task != 0
                && supervisor().terminated() == before + 1
                && !supervisor().may_run(kernel_core::sched::TaskId(ud_task)),
            "entry: a ring-3 ILLEGAL OPCODE (#UD) reaches vector 6 and terminates exactly that task"
        );

        let before_gp = supervisor().terminated();
        let gp_task = run_entry_attack(stub_bytes!(stub_gp_start, stub_gp_end));
        check!(
            gp_task != 0
                && supervisor().terminated() == before_gp + 1
                && !supervisor().may_run(kernel_core::sched::TaskId(gp_task)),
            "entry: a ring-3 PRIVILEGED instruction (#GP) reaches vector 13 and terminates exactly that task"
        );

        check!(
            supervisor().escalations() == 0,
            "entry: neither adversarial entry escalated — a user fault stays a user fault on every vector"
        );

        // The safety net goes back up before anything else runs. A #UD in kernel space must be
        // fatal again; leaving the containment path installed would make a kernel bug look survivable.
        idt::restore_fatal_traps();

        // ...and the machine is still usable after being attacked twice through two different vectors.
        let (held, _va) = run_isolation();
        check!(
            held,
            "entry: after #UD and #GP were both contained, a later ring-3 task still runs to completion"
        );
    }

    let (_denied_info, denied_allowed) = run_process_info(false);
    check!(
        !denied_allowed,
        "process-info: no process.inspect capability is denied at the ring-3 boundary"
    );
    let (granted_info, granted_allowed) = run_process_info(true);
    let (terminated, escalations) = kernel_core::syscall::unpack_process_info(granted_info);
    check!(
        granted_allowed
            && (terminated as usize, escalations as usize)
                == (supervisor().terminated(), supervisor().escalations()),
        "process-info: capability-bound ring-3 query returns live supervisor counters"
    );

    Ok(n)
}
