//! Global Descriptor Table — a fresh flat 64-bit code/data GDT loaded by Aletheia after firmware
//! handoff, replacing the firmware's. Demonstrates real arch bring-up; segments are reloaded via
//! the `x86_64` crate's `CS::set_reg` (which performs the long-mode far-return CS reload correctly).
//!
//! No TSS/IST in this first milestone: the boot path uses a normal, ample stack and never nests
//! faults, so an IST-backed double-fault stack is a documented hardening TODO (P5), not a blocker.

use crate::cell::Racy;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};

static GDT: Racy<GlobalDescriptorTable> = Racy::new(GlobalDescriptorTable::new());

pub fn init() {
    // SAFETY: single-core, init-once, before interrupts are enabled. The GDT is built then loaded;
    // the shared `&'static` borrow for `load()` does not overlap the exclusive build borrow.
    unsafe {
        let gdt = GDT.get_mut();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        GDT.get().load();
        CS::set_reg(code);
        DS::set_reg(data);
        ES::set_reg(data);
        SS::set_reg(data);
    }
}
