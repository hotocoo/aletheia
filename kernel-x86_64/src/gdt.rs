//! Global Descriptor Table — a fresh flat 64-bit GDT loaded by Aletheia after firmware handoff,
//! replacing the firmware's. In addition to the kernel code/data segments it now carries the
//! ring-3 (user) code/data segments and a Task State Segment, which together make an actual
//! privilege boundary possible: the CPU loads `TSS.RSP0` on every ring3->ring0 transition
//! (`int 0x80`, a hardware IRQ taken in ring 3, or a fault), and `iretq` drops to ring 3 using the
//! user selectors. This is the x86-64 twin of the aarch64 backend's EL0/EL1 split (ADR-019).
//!
//! No IST in this milestone: the user-mode path runs the kernel with interrupts masked (IF=0) so a
//! ring3->ring0 entry never nests, and `RSP0` alone is a sound single-level kernel stack. An
//! IST-backed double-fault stack stays a documented P5 hardening TODO.

use crate::cell::Racy;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// Ring-0 stack the CPU loads via `TSS.RSP0` on every ring3->ring0 transition. 16 KiB, 16-aligned.
/// One stack suffices because the kernel runs IF=0 during the user-mode suite, so entries never
/// nest (each fully unwinds back to the scheduler before the next `iretq`).
const KSTACK_SIZE: usize = 16 * 1024;
/// One page BELOW the stack, reserved as a guard (REQ-MM-007, ALET-P1-012). The stack grows down from
/// `RSP0`; without a guard an overflow walks straight into whatever `.bss` put next to it, corrupting it
/// silently. `kmap` leaves this page UNMAPPED, so an overflow takes a #PF at the first byte past the
/// stack instead. Page-aligned so exactly one 4 KiB leaf can be omitted, and it is part of the same
/// static so the linker cannot place anything between the guard and the stack.
#[repr(C, align(4096))]
#[allow(dead_code)] // storage newtype: the bytes ARE the guard + ring-0 stack, used via addr_of!
struct KStack {
    guard: [u8; 4096],
    stack: [u8; KSTACK_SIZE],
}
static mut KSTACK: KStack = KStack {
    guard: [0; 4096],
    stack: [0; KSTACK_SIZE],
};

static TSS: Racy<TaskStateSegment> = Racy::new(TaskStateSegment::new());
static GDT: Racy<GlobalDescriptorTable> = Racy::new(GlobalDescriptorTable::new());

/// The selectors this GDT installed, cached for the user-mode entry/exit path (which builds ring-3
/// interrupt frames referencing the user code/data selectors).
#[derive(Clone, Copy)]
#[allow(dead_code)] // all selectors cached for completeness/debug; only user_code/user_data are read
                    // on the ring-3 frame-build path (kernel_code/kernel_data/tss loaded once in init)
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub user_data: SegmentSelector,
    pub tss: SegmentSelector,
}

static mut SELECTORS: Option<Selectors> = None;

/// The installed selectors. Panics if called before `init`.
pub fn selectors() -> Selectors {
    // SAFETY: written once in `init` before any reader; single-core, no preemption.
    unsafe { *core::ptr::addr_of!(SELECTORS) }.expect("gdt::init must run before selectors()")
}

/// Top of the ring-0 kernel stack (`RSP0`) — highest address, 16-aligned (stack grows down).
pub fn kernel_stack_top() -> u64 {
    // The usable stack starts after the guard page.
    let base = core::ptr::addr_of!(KSTACK) as u64 + 4096;
    (base + KSTACK_SIZE as u64) & !0xF
}

/// The ring-0 stack's guard page — the page `kmap` must leave unmapped (REQ-MM-007, ALET-P1-012).
pub fn kernel_stack_guard() -> usize {
    core::ptr::addr_of!(KSTACK) as usize
}

/// The lowest usable stack byte, immediately above the guard page.
pub fn kernel_stack_low() -> usize {
    core::ptr::addr_of!(KSTACK) as usize + 4096
}

pub fn init() {
    // SAFETY: single-core, init-once, before interrupts are enabled. Each cell is built then
    // published; no two mutable borrows overlap, and no borrow crosses an interrupt.
    unsafe {
        let tss = TSS.get_mut();
        tss.privilege_stack_table[0] = VirtAddr::new(kernel_stack_top()); // RSP0

        let gdt = GDT.get_mut();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(TSS.get()));

        GDT.get().load();
        CS::set_reg(kernel_code);
        DS::set_reg(kernel_data);
        ES::set_reg(kernel_data);
        SS::set_reg(kernel_data);
        load_tss(tss_sel);

        *core::ptr::addr_of_mut!(SELECTORS) = Some(Selectors {
            kernel_code,
            kernel_data,
            user_code,
            user_data,
            tss: tss_sel,
        });
    }
}
