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
mod frames;
mod hal;
mod heap;
mod sbi;
mod smp;
mod trap;
mod usermode;
mod vm;

// Shared, arch-independent Aletheia spine + invariant suite — the SAME source the aarch64 and
// x86-64 kernels compile, pulled in via `#[path]` so every target proves identical invariants
// (no fork, no copy). The shared spine exposes more surface (entity/agent/capability variants,
// provenance fields, IPC message fields) than this particular kernel exercises — the hosted crate
// and the aarch64 `bench` module use the rest — so dead_code is allowed on the shared module to
// keep `clippy -D warnings` clean without touching the shared source.
// Shared, arch-independent Aletheia spine + invariant suite — now a real `kernel-core` dependency
// (defined once there, not `#[path]`-copied per target; gap-register Issue 1). The spine exposes
// more surface than this particular kernel exercises, but as a library crate its `pub` items don't
// trip `dead_code`, so no allow is needed here anymore.
use kernel_core::{selftest, spine};

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

    // Physical memory: bring up the frame allocator over the RAM above the static kernel region.
    frames::init();
    kprintln!(
        "[mm] frame allocator: {} frames ({} MiB) free above kernel, up to {:#x}",
        frames::free_count(),
        frames::free_count() * frames::FRAME_SIZE / (1024 * 1024),
        frames::RAM_END,
    );

    kprintln!("");
    kprintln!("--- invariant selftests (M1 acceptance, re-proved in RISC-V kernel space) ---");
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
            ActiveHal::exit(10 + idx as i32);
        }
    }

    // Physical-memory invariants (riscv64 backend; separate from the shared spine suite).
    kprintln!("");
    kprintln!("--- memory-management selftests (physical frames) ---");
    match frames::selftest() {
        Ok(n) => kprintln!("[mm] ALL {} MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[mm] FAILED at memory invariant {}: {}", idx, name);
            ActiveHal::exit(40 + idx as i32);
        }
    }

    // Virtual memory: build Sv39 tables, enable paging, prove dynamic map/unmap (riscv64 backend).
    kprintln!("");
    kprintln!("--- virtual-memory selftests (Sv39 MMU: identity map + dynamic map/unmap) ---");
    match vm::selftest() {
        Ok(n) => kprintln!("[vm] ALL {} VIRTUAL-MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[vm] FAILED at vm invariant {}: {}", idx, name);
            ActiveHal::exit(60 + idx as i32);
        }
    }

    // U-mode: drop to unprivileged U-mode and prove the capability-gated ecall boundary, hardware
    // address-space isolation, per-process satp spaces, cooperative + timer-preemptive scheduling,
    // and kernel-mediated IPC (riscv64 backend; requires the MMU, enabled above).
    kprintln!("");
    kprintln!("--- user-mode selftests (U-mode boundary: cap-gated ecall + isolation + preemption + IPC) ---");
    match usermode::selftest() {
        Ok(n) => kprintln!("[usermode] ALL {} USER-MODE BOUNDARY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[usermode] FAILED at user-mode invariant {}: {}", idx, name);
            ActiveHal::exit(80 + idx as i32);
        }
    }

    // SMP: start the other harts via SBI HSM and prove the cross-hart substrate (REQ-SMP-002,
    // ADR-021). Skips green on a single-hart machine; the VM gate boots `-smp 4` and asserts the
    // invariant marker below.
    kprintln!("");
    kprintln!(
        "--- SMP selftests (secondary hart bring-up + cross-hart atomics/caps/IPI, real harts) ---"
    );
    match smp::selftest() {
        Ok(0) => {} // single-hart machine — graceful skip, already logged
        Ok(n) => kprintln!("[smp] ALL {} SMP INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[smp] FAILED at SMP invariant {}: {}", idx, name);
            ActiveHal::exit(140 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!(
        "[e2e] PASS — RISC-V S-mode boot + SBI + rdtime + 11 spine + memory + virtual-memory + user-mode invariants"
    );
    kprintln!("[e2e] Aletheia re-proved its invariants on its second first-class target. Halting.");
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}
