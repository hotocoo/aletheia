//! Aletheia microkernel reference (bare-metal aarch64, QEMU virt).
//!
//! Boot -> in-kernel capability-secure spine -> invariant selftests -> IPC benchmark -> exit.
//! Runs entirely in kernel space (EL1) and enforces the same invariants the M1 hosted System
//! Core proved in userspace (ADR-010: contract-honest rehosting on real privilege). The VM's
//! semihosting exit code is the machine-checkable verdict:
//!   0     => all invariants held (e2e PASS)
//!   10+i  => invariant i failed
//!   101   => kernel panic
//!   102   => unexpected CPU exception
#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));

#[macro_use]
mod uart;
mod arch;
mod bench;
mod frames;
mod hal;
mod heap;
mod selftest;
mod semihosting;
mod spine;

/// Kernel entry, called from `_start` (boot.s) after stack + BSS setup.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    use hal::{ActiveHal, Hal};
    kprintln!("========================================");
    kprintln!(
        " Aletheia microkernel — HAL backend: {}",
        ActiveHal::arch_name()
    );
    kprintln!("========================================");
    kprintln!(
        "[hal] first-class targets: AMD64/x86-64, RISC-V  (aarch64 = bootstrap/dev; ADR-019)"
    );
    kprintln!("[boot] OK: stack ready, BSS clear");
    kprintln!("[boot] privilege level: {}", ActiveHal::current_privilege());
    kprintln!("[boot] timer freq: {} Hz", ActiveHal::timer_freq_hz());
    kprintln!("[boot] heap: {} B used after init", heap::used_bytes());

    // Physical memory: bring up the frame allocator over the RAM above the static kernel region.
    frames::init();
    kprintln!(
        "[mm] frame allocator: {} frames ({} MiB) free above kernel, up to {:#x}",
        frames::free_count(),
        frames::free_count() * frames::FRAME_SIZE / (1024 * 1024),
        frames::RAM_END,
    );

    kprintln!("");
    kprintln!("--- invariant selftests (M1 acceptance, re-proved in kernel space) ---");
    match selftest::run() {
        Ok(n) => kprintln!("[selftest] ALL {} INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[selftest] FAILED at invariant {}: {}", idx, name);
            semihosting::exit(10 + idx as i32);
        }
    }

    // Physical-memory invariants (aarch64 dev backend; separate from the shared spine suite).
    kprintln!("");
    kprintln!("--- memory-management selftests (physical frames) ---");
    match frames::selftest() {
        Ok(n) => kprintln!("[mm] ALL {} MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[mm] FAILED at memory invariant {}: {}", idx, name);
            semihosting::exit(40 + idx as i32);
        }
    }

    bench::run();

    kprintln!("");
    kprintln!(
        "[e2e] PASS — boot + spine + {} invariants + memory-management + benchmark complete",
        11
    );
    semihosting::exit(0);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    semihosting::exit(101);
}
