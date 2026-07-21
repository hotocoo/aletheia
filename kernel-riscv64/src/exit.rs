//! Machine exit via the QEMU 'virt' SiFive-test device (MMIO at 0x0010_0000) — a real device present
//! on QEMU `virt` and on SiFive hardware. This, NOT SBI SRST, is the exit channel: SRST can only
//! request a clean shutdown (QEMU exit 0), so it cannot signal a FAILING invariant. The SiFive-test
//! device can encode a code, so `cargo run` / the VM gate can distinguish PASS from a specific FAIL:
//!   code == 0  -> write FINISHER_PASS (0x5555)                => QEMU process exit 0  (e2e PASS)
//!   code  > 0  -> write (code<<16) | FINISHER_FAIL (0x3333)   => QEMU process exit `code`
use core::arch::asm;

const SIFIVE_TEST: usize = 0x0010_0000;
const FINISHER_PASS: u32 = 0x5555;
const FINISHER_FAIL: u32 = 0x3333;

/// Terminate the VM with `code`. Never returns.
pub fn exit(code: i32) -> ! {
    let word: u32 = if code == 0 {
        FINISHER_PASS
    } else {
        ((code as u32 & 0xffff) << 16) | FINISHER_FAIL
    };
    // SAFETY: a single 32-bit MMIO store to the SiFive-test finisher register; QEMU terminates on it.
    unsafe { core::ptr::write_volatile(SIFIVE_TEST as *mut u32, word) };
    // QEMU exits on the store above; this loop only guards against a non-terminating host.
    loop {
        unsafe { asm!("wfi") }
    }
}
