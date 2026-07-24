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
mod semihosting;
mod smp;
mod usermode;
mod virtio;
mod vm;

// The capability-secure spine + the M1 invariant suite are arch-independent and live in
// `kernel-core` — defined once, shared by all three targets (gap-register Issue 1). This kernel
// provides only its own backend (`hal`) + console (`kprintln!`).
use kernel_core::{selftest, spine};

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
    match selftest::run(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
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

    // Virtual memory: build page tables, enable the MMU, prove dynamic map/unmap (aarch64 only).
    kprintln!("");
    kprintln!("--- virtual-memory selftests (MMU: identity map + dynamic map/unmap) ---");
    match vm::selftest() {
        Ok(n) => kprintln!("[vm] ALL {} VIRTUAL-MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[vm] FAILED at vm invariant {}: {}", idx, name);
            semihosting::exit(60 + idx as i32);
        }
    }

    // EL0 user-mode: drop to unprivileged EL0 and prove the capability-gated syscall boundary
    // + hardware address-space isolation (aarch64 dev backend; requires the MMU, enabled above).
    kprintln!("");
    kprintln!(
        "--- user-mode selftests (EL0 privilege boundary: cap-gated syscall + isolation) ---"
    );
    match usermode::selftest() {
        Ok(n) => kprintln!("[usermode] ALL {} EL0-BOUNDARY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[usermode] FAILED at EL0 invariant {}: {}", idx, name);
            semihosting::exit(80 + idx as i32);
        }
    }

    // virtio-blk: the first REAL hardware driver (REQ-DRV-001, ADR-023). Skips green when no disk is
    // attached (bare `cargo run`); the VM gate attaches one and asserts the invariant marker below.
    kprintln!("");
    kprintln!(
        "--- virtio-blk selftests (real driver: discovery + virtqueue I/O + journal over storage) ---"
    );
    match virtio::selftest() {
        Ok(0) => {} // no device attached — graceful skip, already logged
        Ok(n) => kprintln!("[virtio] ALL {} VIRTIO-BLK INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[virtio] FAILED at virtio invariant {}: {}", idx, name);
            semihosting::exit(120 + idx as i32);
        }
    }

    // SMP: power on the other CPUs via PSCI and prove the cross-core substrate (REQ-SMP-002,
    // ADR-028). Skips green on a single-CPU machine (bare `cargo run`); the VM gate boots
    // `-smp 4` and asserts the invariant marker below.
    kprintln!("");
    kprintln!(
        "--- SMP selftests (secondary bring-up + cross-core atomics/caps/IPI, real cores) ---"
    );
    match smp::selftest() {
        Ok(0) => {} // single-CPU machine — graceful skip, already logged
        Ok(n) => kprintln!("[smp] ALL {} SMP INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[smp] FAILED at SMP invariant {}: {}", idx, name);
            semihosting::exit(140 + idx as i32);
        }
    }

    bench::run();

    kprintln!("");
    kprintln!(
        "[e2e] PASS — boot + spine + {} invariants + memory-management + virtual-memory + user-mode + benchmark complete",
        11
    );
    semihosting::exit(0);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    semihosting::exit(101);
}
