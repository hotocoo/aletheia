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
#[repr(align(16))]
struct KStack([u8; KSTACK_SIZE]);
static mut KSTACK: KStack = KStack([0; KSTACK_SIZE]);

static TSS: Racy<TaskStateSegment> = Racy::new(TaskStateSegment::new());
static GDT: Racy<GlobalDescriptorTable> = Racy::new(GlobalDescriptorTable::new());

/// The selectors this GDT installed, cached for the user-mode entry/exit path (which builds ring-3
/// interrupt frames referencing the user code/data selectors).
#[derive(Clone, Copy)]
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
    let base = core::ptr::addr_of!(KSTACK) as u64;
    (base + KSTACK_SIZE as u64) & !0xF
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
