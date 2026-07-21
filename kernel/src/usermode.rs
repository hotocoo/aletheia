//! EL0 user-mode + the capability-gated syscall boundary (PRD P5, mm brick 3).
//!
//! WHY THIS MATTERS: until now every invariant was re-proved *in kernel space* (EL1). The
//! benchmark's own honesty note says it: "the measured loop runs in EL1 and crosses no
//! privilege/address-space boundary." So "isolation" was logical, not enforced by hardware —
//! a bug at EL1 could touch anything. This module makes the privilege boundary REAL: it drops
//! the CPU to **EL0** (unprivileged), runs a genuinely less-privileged instruction stream in
//! its own EL0-only pages, and lets that stream reach the OS through *exactly one* door — an
//! `svc` trap that lands in the EL1 vector and is authorized by the **same `CapEngine`** the
//! deterministic pipeline uses. Same authority mechanism, now at the hardware boundary.
//!
//! DESIGN — one-shot excursions, not a scheduler (contract-honest, ADR-010). This is a test
//! harness for the boundary, not multitasking. Each trial: the kernel saves its callee-saved
//! context, `eret`s to EL0 with a tiny position-independent stub, the stub issues one `svc`
//! (or faults), the 0x400 handler dispatches it and then resumes the kernel — it never returns
//! to EL0. No per-task trap frame, no context switch (those are the follow-on bricks). Every
//! line executes under QEMU and is asserted by `usermode::selftest()`; an *unexpected* fault
//! is still fatal (`exit 102`), so a real bug can never masquerade as a passing test.
//!
//! SCOPE: aarch64 dev backend only (like `frames.rs`/`vm.rs`); the shared `spine.rs` is
//! untouched. Higher-half/per-process address spaces and the x86-64/RISC-V EL0 backends are
//! the documented follow-on. Requires the MMU already enabled (`vm::enable`) so that EL0-only
//! page permissions (AP) are enforced.
use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Store, Target};
use crate::{frames, vm};
use alloc::vec::Vec;
use core::arch::{asm, global_asm};

global_asm!(
    r#"
.section .text

// fn enter_user(entry: usize /*x0*/, user_sp: usize /*x1*/, x0val: u64 /*x2*/, x8val: u64 /*x3*/) -> u64
// Save the kernel's callee-saved context (so the 0x400 handler can resume us), then ERET to
// EL0 with the user PC/SP and x0/x8 primed. The general registers survive ERET unchanged — it
// only swaps PC, PSTATE, and (per SPSR.M) the stack pointer.
.global enter_user
enter_user:
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
    msr     elr_el1, x0             // return-to-EL0 PC
    mov     x11, #0x3C0             // SPSR: M=EL0t (AArch64) + DAIF masked
    msr     spsr_el1, x11
    msr     sp_el0, x1              // EL0 stack pointer
    mov     x0, x2                  // user x0
    mov     x8, x3                  // user x8 (syscall number)
    isb
    eret

// The 0x400 handler branches here (at EL1) with x0 = result. Restore the kernel context saved
// above and return to enter_user's caller — a non-local resume that discards the EL0 excursion.
.global enter_user_return
enter_user_return:
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

// Vector 0x400 (Lower EL, AArch64, Synchronous) routes here — an EL0 `svc` AND an EL0 fault
// land at the same vector, so decode ESR_EL1.EC: SVC(0x15) -> el0_syscall(num=x8, arg=x0);
// Data Abort from a lower EL (0x24) -> el0_data_abort(FAR). Anything else is a real bug ->
// default_exception (exit 102).
.global el0_sync_entry
el0_sync_entry:
    mrs     x9, esr_el1
    lsr     x9, x9, #26
    and     x9, x9, #0x3f
    cmp     x9, #0x15
    b.eq    10f
    cmp     x9, #0x24
    b.eq    20f
    b       default_exception
10: mov     x1, x0                  // syscall arg  -> el0_syscall 2nd param
    mov     x0, x8                  // syscall num  -> el0_syscall 1st param
    bl      el0_syscall
    b       enter_user_return
20: mrs     x0, far_el1             // faulting VA -> el0_data_abort param
    bl      el0_data_abort
    b       enter_user_return
"#
);

extern "C" {
    /// Drop to EL0 at `entry` with `user_sp`, priming user `x0`/`x8`; returns when the 0x400
    /// handler resumes the kernel with a result value.
    fn enter_user(entry: usize, user_sp: usize, x0val: u64, x8val: u64) -> u64;
    /// The EL1 vector table (from `vectors.s`); its address goes into `VBAR_EL1`.
    static exc_vectors: u8;
}

/// Callee-saved kernel context stash used by the asm `enter_user`/`enter_user_return` pair.
/// 13 × u64: x19..x30 (pairs at byte offsets 0..80) then SP at offset 96.
#[no_mangle]
static mut KERNEL_CTX: [u64; 13] = [0; 13];

/// Syscall numbers the EL0 boundary understands. Anything else is denied (fail closed).
const SYS_EMIT: u64 = 1;

/// Virtual addresses for the user process's pages — past the identity-mapped RAM (so they are
/// unmapped until we map them, proving these are real EL0-only mappings, not identity RAM).
const USER_CODE_VA: usize = 0x5000_0000;
const USER_STACK_VA: usize = 0x5000_1000;
const USER_STACK_TOP: usize = USER_STACK_VA + frames::FRAME_SIZE; // 16-byte aligned page top

/// The live trial state, reached by the `#[no_mangle]` handlers (which the asm calls with no
/// context pointer). Single-threaded kernel (secondary harts parked), one excursion at a time.
struct Trial {
    engine: CapEngine,
    store: Store,
    caps: Vec<CapToken>,
    action: &'static str,
    /// When set, a Data Abort from EL0 is the *expected* isolation test, not a fatal bug.
    armed: bool,
    // --- outcomes, read back after `enter_user` returns ---
    allowed: bool,
    isolation_held: bool,
    fault_va: usize,
}

static mut CURRENT: Option<Trial> = None;

/// SAFETY: single-threaded kernel; `CURRENT` is set by `run_*` immediately before `enter_user`
/// and mutated only by the handler that same excursion drives. No concurrent access exists.
#[inline]
fn current() -> Option<&'static mut Trial> {
    unsafe { (*core::ptr::addr_of_mut!(CURRENT)).as_mut() }
}

/// SVC dispatch — the capability-gated syscall. Authorizes through the SAME `CapEngine` the
/// deterministic pipeline uses, against the process's granted caps. Allow ⇒ perform the effect
/// (record an event, actor = the EL0 process) and return 0; Deny ⇒ return −1 with zero effect.
#[no_mangle]
pub extern "C" fn el0_syscall(num: u64, _arg: u64) -> u64 {
    let t = match current() {
        Some(t) => t,
        None => return u64::MAX,
    };
    if num != SYS_EMIT {
        return u64::MAX; // unknown syscall — fail closed
    }
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

/// Data-abort dispatch. If the trial armed the isolation test, an EL0 fault is the expected
/// proof that EL0 cannot reach kernel memory: record it and resume. Any UNEXPECTED abort is a
/// real fault and stays fatal (`exit 102`) so bugs cannot hide behind this handler.
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

/// Point `VBAR_EL1` at our vector table so an EL0 `svc`/fault traps to `el0_sync_entry`. The
/// benchmark also does this later; setting it twice is harmless (idempotent).
fn install_vectors() {
    // SAFETY: `exc_vectors` is a valid, 2 KiB-aligned in-image vector table; writing VBAR_EL1
    // + `isb` is the architected way to install it at EL1.
    unsafe {
        let addr = core::ptr::addr_of!(exc_vectors) as u64;
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

/// Map a fresh frame at `va` as EL0-executable code, writing `code` (aarch64 machine words)
/// into it first. Returns the backing frame (caller unmaps+frees).
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

/// Machine code for a leaf EL0 stub that issues one syscall then parks: `svc #0 ; b .`
/// (the syscall number/arg arrive in x8/x0, primed by `enter_user`).
const STUB_SYSCALL: [u32; 2] = [0xD400_0001, 0x1400_0000];
/// EL0 stub that reads the kernel address handed to it in x0, then parks: `ldr x1,[x0] ; b .`
const STUB_READ_X0: [u32; 2] = [0xF940_0001, 0x1400_0000];

/// Run one EL0 syscall excursion. `grant` decides whether the process holds the `event.emit`
/// capability. Returns `(authorized, event_count_after)`.
fn run_syscall(grant: bool) -> (bool, usize) {
    let root = vm::active_root();
    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint("el0-process", "event.emit", Scope::All, Constraints::none()));
    }
    // SAFETY: single-threaded; installs the trial the handler will read this excursion.
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = Some(Trial {
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

    // SAFETY: code/stack pages are mapped EL0-accessible; the stub only issues `svc`.
    let _ = unsafe { enter_user(USER_CODE_VA, USER_STACK_TOP, 0, SYS_EMIT) };

    drop_user_page(root, USER_STACK_VA, stack);
    drop_user_page(root, USER_CODE_VA, code);

    // SAFETY: excursion complete; read back and take the trial.
    // SAFETY: single-threaded; the excursion is complete, no other access to CURRENT exists.
    let t = unsafe { (*core::ptr::addr_of_mut!(CURRENT)).take() }.expect("trial present");
    (t.allowed, t.store.event_count())
}

/// Run the isolation excursion: hand the EL0 stub a kernel-only address and prove it cannot
/// read it — the access must fault and be contained. Returns `(isolation_held, fault_va)`.
fn run_isolation() -> (bool, usize) {
    let root = vm::active_root();
    // A kernel-only address (this static lives in identity-mapped RAM, AP = EL1-only).
    let kernel_va = core::ptr::addr_of!(KERNEL_CTX) as u64;
    // SAFETY: single-threaded; arm the isolation test so the abort handler treats the fault as
    // expected rather than fatal.
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = Some(Trial {
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

    // SAFETY: stub reads the kernel address in x0; EL0 permission fault is expected + armed.
    let _ = unsafe { enter_user(USER_CODE_VA, USER_STACK_TOP, kernel_va, 0) };

    drop_user_page(root, USER_STACK_VA, stack);
    drop_user_page(root, USER_CODE_VA, code);

    // SAFETY: single-threaded; the excursion is complete, no other access to CURRENT exists.
    let t = unsafe { (*core::ptr::addr_of_mut!(CURRENT)).take() }.expect("trial present");
    (t.isolation_held, t.fault_va)
}

/// Prove the EL0 boundary invariants live. `Ok(n)` all passed; `Err((idx,name))` = failure.
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
        held && fault_va == core::ptr::addr_of!(KERNEL_CTX) as usize,
        "el0: EL0 read of kernel memory faults — address-space isolation holds"
    );

    Ok(n)
}
