//! Interrupt Descriptor Table — CPU exception handlers + the IRQ0 (PIT timer) handler.
//!
//! Mandatory before `sti`: after `ExitBootServices` the firmware's IDT is gone, so any exception
//! without our handler triple-faults (VM reset). The fault handlers print a precise diagnostic to
//! the console and exit with a distinct code, turning would-be triple-faults into a legible failure
//! the smoke test can read. Handlers use the nightly `x86-interrupt` calling convention (the
//! compiler emits the correct interrupt prologue/epilogue + `iretq`).

use crate::cell::Racy;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::{PrivilegeLevel, VirtAddr};

/// IRQ0 (timer) is remapped to vector 0x20 by the PIC.
pub const TIMER_VECTOR: u8 = 0x20;

/// The software-interrupt vector the ring-3 syscall door uses (`int 0x80`). Its IDT gate is
/// installed with DPL=3 so an unprivileged task may invoke it; every other user vector stays DPL=0.
pub const SYSCALL_VECTOR: u8 = 0x80;

static IDT: Racy<InterruptDescriptorTable> = Racy::new(InterruptDescriptorTable::new());

pub fn init() {
    // SAFETY: single-core, init-once, before `sti`.
    unsafe {
        let idt = IDT.get_mut();
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.general_protection_fault.set_handler_fn(general_protection);
        idt.page_fault.set_handler_fn(page_fault);
        idt.double_fault.set_handler_fn(double_fault);
        idt[TIMER_VECTOR].set_handler_fn(timer);
        IDT.get().load();
    }
}

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    kprintln!("[cpu] #BP at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    kprintln!("[cpu] #UD (invalid opcode) at {:#x}", frame.instruction_pointer.as_u64());
    crate::exit::exit(105);
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, err: u64) {
    kprintln!("[cpu] #GP err={:#x} at {:#x}", err, frame.instruction_pointer.as_u64());
    crate::exit::exit(103);
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, err: PageFaultErrorCode) {
    kprintln!("[cpu] #PF {:?} at {:#x}", err, frame.instruction_pointer.as_u64());
    crate::exit::exit(104);
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, _err: u64) -> ! {
    kprintln!("[cpu] #DF (double fault) at {:#x}", frame.instruction_pointer.as_u64());
    crate::exit::exit(102)
}

extern "x86-interrupt" fn timer(_frame: InterruptStackFrame) {
    crate::pit::tick();
    crate::pic::eoi(TIMER_VECTOR);
}

/// Repoint the three vectors the user-mode suite drives at its own register-exact assembly entries
/// (which the `x86-interrupt` ABI cannot express — they context-switch whole register files):
///   * `int 0x80` -> `syscall_entry`, gate DPL=3 so ring 3 can invoke it (the one syscall door);
///   * IRQ0 (`TIMER_VECTOR`) -> `timer_entry`, so a timer taken in ring 3 preempts the running task;
///   * `#PF` -> `pf_entry`, so an armed isolation trial contains the fault instead of exiting.
///
/// The CPU reads the in-memory IDT on each interrupt, so mutating the loaded table takes effect
/// without a reload. Called once, from `usermode::selftest`, with interrupts already masked.
///
/// # Safety
/// Each address must be a valid raw interrupt entry point that saves/restores state itself and ends
/// by unwinding to the scheduler; the caller runs single-core with IF=0.
pub unsafe fn install_usermode(syscall_entry: u64, timer_entry: u64, pf_entry: u64) {
    let idt = IDT.get_mut();
    idt[SYSCALL_VECTOR]
        .set_handler_addr(VirtAddr::new(syscall_entry))
        .set_privilege_level(PrivilegeLevel::Ring3);
    idt[TIMER_VECTOR].set_handler_addr(VirtAddr::new(timer_entry));
    idt.page_fault.set_handler_addr(VirtAddr::new(pf_entry));
}
