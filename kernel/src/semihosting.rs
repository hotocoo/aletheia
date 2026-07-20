//! ARM semihosting — the kernel's only channel to signal a pass/fail exit code to the
//! host test harness. `exit(0)` => QEMU terminates with status 0 (e2e PASS); any nonzero
//! code => FAIL. Requires `-semihosting-config enable=on` on the QEMU command line.
use core::arch::asm;

const SYS_EXIT_EXTENDED: u64 = 0x20;
const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x2_0026;

/// Terminate the VM with `code`. Never returns.
pub fn exit(code: i32) -> ! {
    let block = [ADP_STOPPED_APPLICATION_EXIT, code as u64];
    unsafe {
        asm!(
            "hlt #0xf000",
            in("x0") SYS_EXIT_EXTENDED,
            in("x1") block.as_ptr(),
            options(nostack),
        );
    }
    // QEMU exits on the trap above; this loop only guards against a non-terminating host.
    loop {
        unsafe { asm!("wfe") }
    }
}
