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
    j   _trap_handler
"#
);

#[no_mangle]
extern "C" fn _trap_handler() -> ! {
    let scause: usize;
    let sepc: usize;
    let stval: usize;
    unsafe {
        asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack));
        asm!("csrr {}, sepc", out(reg) sepc, options(nomem, nostack));
        asm!("csrr {}, stval", out(reg) stval, options(nomem, nostack));
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
