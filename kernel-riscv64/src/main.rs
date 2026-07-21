//! Aletheia microkernel — bare-metal RISC-V (RV64GC, QEMU virt), the SECOND first-class target
//! (ADR-019). OpenSBI (M-mode) hands off to us in S-mode; we install a trap vector, prove the S->M
//! SBI boundary, show the `rdtime` counter is live, then re-prove the M1 capability-secure spine
//! invariants IN KERNEL SPACE — identical invariants to the aarch64 and x86-64 backends, from the
//! SAME shared `spine.rs` / `selftest.rs` (pulled via `#[path]`, no fork). The VM's process exit
//! code (via the SiFive-test device) is the machine-checkable verdict (ADR-010: this runs):
//!   0     => all invariants held (e2e PASS)
//!   10+i  => invariant i failed
//!   101   => kernel panic
//!   102   => unexpected S-mode trap
#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(include_str!("boot.s"));

#[macro_use]
mod console;
mod arch;
mod exit;
mod hal;
mod heap;
mod sbi;
mod trap;

// Shared, arch-independent Aletheia spine + invariant suite — the SAME source the aarch64 and
// x86-64 kernels compile, pulled in via `#[path]` so every target proves identical invariants
// (no fork, no copy). The shared spine exposes more surface (entity/agent/capability variants,
// provenance fields, IPC message fields) than this particular kernel exercises — the hosted crate
// and the aarch64 `bench` module use the rest — so dead_code is allowed on the shared module to
// keep `clippy -D warnings` clean without touching the shared source.
#[path = "../../kernel/src/selftest.rs"]
mod selftest;
#[allow(dead_code)]
#[path = "../../kernel/src/spine.rs"]
mod spine;

/// Kernel entry, called from `_start` (boot.s) after stack + BSS setup.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    use hal::{ActiveHal, Hal};

    trap::init();

    kprintln!("");
    kprintln!("========================================");
    kprintln!(
        " Aletheia microkernel — HAL backend: {}",
        ActiveHal::arch_name()
    );
    kprintln!("========================================");
    kprintln!(
        "[hal] first-class targets: AMD64/x86-64, RISC-V  (aarch64 = bootstrap/dev; ADR-019)"
    );
    kprintln!("[boot] OK: stack ready, BSS clear, stvec installed");
    kprintln!(
        "[boot] privilege level: {} (S-mode; entered via OpenSBI handoff)",
        ActiveHal::current_privilege()
    );
    kprintln!(
        "[boot] timer freq: {} Hz (rdtime `time` CSR)",
        ActiveHal::timer_freq_hz()
    );

    // Prove the S->M SBI firmware boundary works (the RISC-V privilege-crossing interface).
    sbi::probe();

    // Prove the time counter is actually advancing (interrupts stay off; polled monotonic read).
    let t0 = ActiveHal::timer_ticks();
    let mut t1 = t0;
    while t1 == t0 {
        t1 = ActiveHal::timer_ticks();
    }
    kprintln!(
        "[timer] rdtime advancing: {} -> {} (~{} ns elapsed)",
        t0,
        t1,
        ActiveHal::ticks_to_ns(t1 - t0)
    );
    kprintln!("[boot] heap: {} B used after init", heap::used_bytes());

    kprintln!("");
    kprintln!("--- invariant selftests (M1 acceptance, re-proved in RISC-V kernel space) ---");
    match selftest::run() {
        Ok(n) => kprintln!("[selftest] ALL {} INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[selftest] FAILED at invariant {}: {}", idx, name);
            ActiveHal::exit(10 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!("[e2e] PASS — RISC-V S-mode boot + SBI + rdtime + 11 spine invariants");
    kprintln!("[e2e] Aletheia re-proved its invariants on its second first-class target. Halting.");
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}
