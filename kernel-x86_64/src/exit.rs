//! Machine exit. Tries QEMU's `isa-debug-exit` device (I/O port 0xF4) so the smoke test gets a
//! machine-checkable process exit code, then falls through to a permanent `cli; hlt` halt so the
//! SAME binary is also safe on firmware without that device (VMware, real hardware).

use x86_64::instructions::port::Port;

/// Terminate with `code`. `isa-debug-exit` computes the QEMU process exit as `(value << 1) | 1`,
/// so we encode success (`code == 0`) as value 0x10 => QEMU exit 33, which QEMU never emits on its
/// own (its self-generated errors are exit 1). Failure codes map to 0x10 + code.
pub fn exit(code: i32) -> ! {
    let value: u32 = if code == 0 { 0x10 } else { 0x10 + code as u32 };
    // SAFETY: writing the isa-debug-exit port is side-effect-only; absent the device it's a no-op.
    unsafe { Port::<u32>::new(0xf4).write(value) };
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}
