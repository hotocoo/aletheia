//! A USB keyboard as console input on aarch64 (REQ-DRV-015, ADR-256).
//!
//! The boot suite proves the xHCI controller and halts it (ADR-253); an interactive machine then
//! re-opens it and keeps the first boot keyboard here. The console's `getc` asks this module for a
//! byte after the UART ring comes up empty, and only then is the controller polled: device
//! draining stays in the foreground, never in the timer handler (ADR-080). What wakes the console
//! to ask is the EL1 physical timer `conirq::init` arms while a keyboard is held.

use kernel_core::xhci::KeyboardInput;

type Input = KeyboardInput<crate::virtio::Aarch64Virtio, kernel_core::nvme::MmioRegs>;

/// The kept keyboard, written once on the boot path.
static mut KEPT: Option<Input> = None;

/// Keep `input` as console input.
///
/// # Safety
/// Called once, on the boot path, before any other context can reach the static.
pub unsafe fn keep(input: Input) {
    (*core::ptr::addr_of_mut!(KEPT)) = Some(input);
}

/// Whether a USB keyboard is held (and has not been halted by a wrong answer).
pub fn is_armed() -> bool {
    // SAFETY: written once on the boot path; read by the console's single thread.
    unsafe { (*core::ptr::addr_of!(KEPT)).as_ref() }.is_some_and(|i| i.failed().is_none())
}

/// The next byte the USB keyboard typed, if any.
pub fn getc() -> Option<u8> {
    // SAFETY: only the console's foreground loop calls this; nothing else touches the static after
    // boot, and no interrupt handler reaches it.
    unsafe { (*core::ptr::addr_of_mut!(KEPT)).as_mut() }?.getc()
}
