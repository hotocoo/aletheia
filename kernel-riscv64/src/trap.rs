//! S-mode trap vector. Any unexpected trap (exception or interrupt) is fatal in this boot-run-exit
//! reference kernel: the handler reports `scause`/`sepc` and exits the VM with status 102, mirroring
//! the aarch64 kernel's `default_exception` and the x86-64 kernel's fatal exception handlers. The
//! selftests never trap; installing `stvec` is correct kernel hygiene and makes any regression loud.
use core::arch::asm;

// A 4-byte-aligned trampoline (stvec[1:0]==0 selects Direct mode) that jumps to the Rust handler.
core::arch::global_asm!(
    r#"
    .section .text
    .balign 4
    .global _trap_entry
_trap_entry:
    // Save every caller-saved register a Rust `extern "C"` handler may clobber, so the interrupted
    // code resumes unchanged. Callee-saved registers the handler preserves itself; there is no FP
    // state in these kernels. `sepc`/`sstatus` are left as the hardware set them — nothing here
    // re-enables interrupts, so the frame cannot nest and `sret` returns to the right place.
    addi sp, sp, -128
    sd   ra, 0(sp)
    sd   t0, 8(sp)
    sd   t1, 16(sp)
    sd   t2, 24(sp)
    sd   t3, 32(sp)
    sd   t4, 40(sp)
    sd   t5, 48(sp)
    sd   t6, 56(sp)
    sd   a0, 64(sp)
    sd   a1, 72(sp)
    sd   a2, 80(sp)
    sd   a3, 88(sp)
    sd   a4, 96(sp)
    sd   a5, 104(sp)
    sd   a6, 112(sp)
    sd   a7, 120(sp)
    call _trap_handler
    ld   ra, 0(sp)
    ld   t0, 8(sp)
    ld   t1, 16(sp)
    ld   t2, 24(sp)
    ld   t3, 32(sp)
    ld   t4, 40(sp)
    ld   t5, 48(sp)
    ld   t6, 56(sp)
    ld   a0, 64(sp)
    ld   a1, 72(sp)
    ld   a2, 80(sp)
    ld   a3, 88(sp)
    ld   a4, 96(sp)
    ld   a5, 104(sp)
    ld   a6, 112(sp)
    ld   a7, 120(sp)
    addi sp, sp, 128
    sret
"#
);

/// S-mode trap dispatch.
///
/// Returns for exactly one cause — the console's external interrupt (REQ-CON-002, ADR-045) — and is
/// fatal for everything else, which is what this vector always was. Widening a fatal handler must not
/// quietly absorb the traps nobody expected: an unrecognised cause, or an external interrupt no
/// device here claims, still reports and exits 102.
#[no_mangle]
extern "C" fn _trap_handler() {
    let scause: usize;
    let sepc: usize;
    let stval: usize;
    unsafe {
        asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack));
        asm!("csrr {}, sepc", out(reg) sepc, options(nomem, nostack));
        asm!("csrr {}, stval", out(reg) stval, options(nomem, nostack));
    }
    if scause == crate::conirq::SCAUSE_S_EXTERNAL && crate::conirq::on_external_irq() {
        return;
    }
    // The live desktop's tick (ADR-085). Only an interactive boot ever enables `sie.STIE`, so a
    // gate build reaching this cause would be the bug this vector is fatal for.
    #[cfg(feature = "interactive")]
    if scause == crate::conirq::SCAUSE_S_TIMER && crate::conirq::on_timer_irq() {
        return;
    }
    kprintln!(
        "[TRAP] unexpected S-mode trap: scause={:#x} sepc={:#x} stval={:#x}",
        scause,
        sepc,
        stval
    );
    crate::exit::exit(102)
}

/// Install the trap vector in `stvec` (Direct mode).
pub fn init() {
    extern "C" {
        fn _trap_entry();
    }
    // SAFETY: `_trap_entry` is a 4-byte-aligned label; writing its address to stvec with the low two
    // bits clear selects Direct mode, the documented S-mode trap-entry convention.
    unsafe {
        asm!("csrw stvec, {}", in(reg) _trap_entry as *const () as usize, options(nomem, nostack))
    };
}
