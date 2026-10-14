//! A USB keyboard as console input on x86-64 (REQ-DRV-014, ADR-255).
//!
//! The boot suite proves the xHCI controller and halts it (ADR-253); an interactive machine then
//! re-opens it and keeps the first boot keyboard here. The console's `getc` asks this module for a
//! byte after the serial/PS/2 ring comes up empty, and only then is the controller polled: device
//! draining stays in the foreground, never in the timer handler (ADR-080). What wakes the console
//! to ask is the PIT tick `conirq::init` keeps running while a keyboard is held.

use kernel_core::xhci::KeyboardInput;

use crate::pci::Bdf;

type Input = KeyboardInput<crate::virtio::X86Virtio, kernel_core::nvme::MmioRegs>;

/// The kept keyboard and the PCI function its controller is, written once on the boot path.
static mut KEPT: Option<(Bdf, Input)> = None;

/// Keep `input` as console input.
///
/// # Safety
/// Called once, on the boot path, before any other context can reach the static.
pub unsafe fn keep(bdf: Bdf, input: Input) {
    (*core::ptr::addr_of_mut!(KEPT)) = Some((bdf, input));
}

/// Whether a USB keyboard is held (and has not been halted by a wrong answer).
pub fn is_armed() -> bool {
    // SAFETY: written once on the boot path; read by the console's single thread.
    unsafe { (*core::ptr::addr_of!(KEPT)).as_ref() }.is_some_and(|(_, i)| i.failed().is_none())
}

/// The next byte the USB keyboard typed, if any.
pub fn getc() -> Option<u8> {
    // SAFETY: only the console's foreground loop calls this; nothing else touches the static after
    // boot, and no interrupt handler reaches it.
    let (_, input) = unsafe { (*core::ptr::addr_of_mut!(KEPT)).as_mut() }?;
    input.getc()
}

/// The kept controller's function and DMA grants, for the VT-d table (ADR-075): it keeps fetching
/// its rings after enforcement is on.
pub fn grants() -> Option<(Bdf, alloc::vec::Vec<kernel_core::dma::Grant>)> {
    // SAFETY: boot path, single-threaded; read of a static written once before.
    unsafe { (*core::ptr::addr_of!(KEPT)).as_ref() }.map(|(b, i)| (*b, i.dma_grants()))
}
