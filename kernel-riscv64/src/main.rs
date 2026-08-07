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
mod conirq;
mod exit;
mod frames;
mod hal;
mod heap;
mod sbi;
mod shellio;
mod smp;
mod trap;
mod usermode;
mod virtio;
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
    if !frames::init() {
        kprintln!("[mm] FATAL: frame ownership table does not cover the pool");
        ActiveHal::exit(39);
    }
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

    // What a device is allowed to touch (REQ-DRV-006, ADR-043). Every driver here hands a device a RAW
    // physical address; since bus-master was enabled (ADR-037) a wrong address is a device writing wherever
    // the number points. This is the software boundary the kernel can enforce without an IOMMU: a frame is
    // registered before any descriptor may name it, the kernel image is never a legal target, and an
    // address nobody registered is refused.
    kprintln!("");
    kprintln!("--- DMA-boundary selftests (what a device may be told about) ---");
    match kernel_core::dma::selftest(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[dma] ALL {} DMA-BOUNDARY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[dma] FAILED at DMA invariant {}: {}", idx, name);
            ActiveHal::exit(240 + idx as i32);
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

    // virtio-blk: a REAL block device on a FIRST-CLASS target (REQ-DRV-004, ADR-036). The driver is
    // the shared `kernel_core::virtioblk`; this target supplies only its MMIO window, its frame
    // allocator and its fence. Skips green when no disk is attached (bare `cargo run`); the VM gate
    // attaches one and requires the marker. The suite ends by proving the whole filesystem namespace
    // over the real device, through the virtqueue.
    kprintln!("");
    kprintln!(
        "--- virtio-blk selftests (real driver: discovery + virtqueue I/O + journal + filesystem) ---"
    );
    match virtio::selftest() {
        Ok(0) => {} // no device attached — graceful skip, already logged
        Ok(n) => kprintln!("[virtio] ALL {} VIRTIO-BLK INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[virtio] FAILED at virtio invariant {}: {}", idx, name);
            ActiveHal::exit(180 + idx as i32);
        }
    }

    // Filesystem: the named-object namespace over the journaled block store (REQ-FS-001, ADR-035).
    // The namespace is arch-independent, so every target proves the SAME behaviors over a RAM-disk
    // device; aarch64 additionally proves them over the real virtio-blk driver.
    kprintln!("");
    kprintln!(
        "--- filesystem selftests (named objects over the journal: atomic create/remove) ---"
    );
    let mut disk = kernel_core::storage::MemBlockDevice::new(kernel_core::fs::FILE_DATA_START + 64);
    match kernel_core::fs::selftest_on(&mut disk, |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[fs] ALL {} FILESYSTEM INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[fs] FAILED at filesystem invariant {}: {}", idx, name);
            ActiveHal::exit(160 + idx as i32);
        }
    }

    // Durable store: the OS REMEMBERS (REQ-STOR-003, ADR-038). The spine was rebuilt in RAM every
    // boot; now it is encoded into one filesystem object, updated in ONE atomic transaction, and
    // re-verified against each entity's content address on load — a flipped bit is a refusal, not
    // silently-accepted state. Proved here over a RAM disk on every target.
    kprintln!("");
    kprintln!(
        "--- durable-store selftests (the spine survives: content-verified, atomically saved) ---"
    );
    let mut store_disk =
        kernel_core::storage::MemBlockDevice::new(kernel_core::fs::FILE_DATA_START + 64);
    match kernel_core::persist::selftest_on(&mut store_disk, |n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[persist] ALL {} DURABLE-STORE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[persist] FAILED at durable-store invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(200 + idx as i32);
        }
    }

    // The cross-reboot claim, on REAL hardware (REQ-STOR-003, ADR-038). The persistent medium is the
    // SECOND disk: the scratch one above was reformatted by the destructive suites, this one is never
    // wiped. The boot gate boots twice against the same image file, so the second boot must FIND and
    // verify what the first wrote — the difference between "the OS can write" and "the OS remembers".
    kprintln!("");
    match virtio::persistent_device() {
        Some(mut medium) => match kernel_core::persist::open_and_witness(&mut medium) {
            Ok((boot, verified)) => kprintln!(
                "[persist] PERSISTENT MEDIUM: boot #{}, {} entities verified from earlier boots",
                boot,
                verified
            ),
            Err(e) => {
                kprintln!("[persist] PERSISTENT MEDIUM FAILED: {:?}", e);
                ActiveHal::exit(210);
            }
        },
        None => kprintln!("[persist] no persistent medium attached (skipped)"),
    }

    // Networking (REQ-NET-001/002, ADR-041): the first real slice — a virtio-net device, and enough
    // protocol to prove the path against something that ANSWERS. A transmit-only driver proves nothing, so
    // the suite ARPs QEMU's gateway and pings it: the reply must carry the address asked about, and the
    // echo must come back with matching id, sequence and payload, its checksums verified.
    kprintln!("");
    kprintln!("--- network selftests (virtio-net: ARP + ICMP echo against the gateway) ---");
    match virtio::network_device() {
        None => kprintln!("[net] no network device attached (skipped)"),
        Some(Err(e)) => {
            kprintln!("[net] device init FAILED: {:?}", e);
            ActiveHal::exit(220);
        }
        Some(Ok(net)) => match kernel_core::virtionet::net_suite(net, |n, passed, name| {
            if passed {
                kprintln!("  [pass {:>2}] {}", n, name);
            } else {
                kprintln!("  [FAIL {:>2}] {}", n, name);
            }
        }) {
            Ok(n) => kprintln!("[net] ALL {} NETWORK INVARIANTS HOLD", n),
            Err((idx, name)) => {
                kprintln!("[net] FAILED at network invariant {}: {}", idx, name);
                ActiveHal::exit(220 + idx as i32);
            }
        },
    }

    // The interactive console (REQ-CON-001, ADR-044): the subsystem that lets the machine stay up
    // and answer a human, proved here the same way everything else is — a scripted session against
    // a real namespace, so the gate covers the code an interactive boot runs.
    kprintln!("");
    kprintln!("--- input-ring selftests (what an interrupt hands the shell) ---");
    match shellio::ring_selftest() {
        Ok(n) => kprintln!("[conring] ALL {} INPUT-RING INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[conring] FAILED at input-ring invariant {}: {}", idx, name);
            ActiveHal::exit(230 + idx as i32);
        }
    }

    kprintln!("--- console selftests (line editing + command dispatch over the namespace) ---");
    match shellio::selftest() {
        Ok(n) => kprintln!("[console] ALL {} CONSOLE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[console] FAILED at console invariant {}: {}", idx, name);
            ActiveHal::exit(250 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!(
        "[e2e] PASS — RISC-V S-mode boot + SBI + rdtime + 11 spine + memory + virtual-memory + user-mode + filesystem + console invariants"
    );
    kprintln!("[e2e] Aletheia re-proved its invariants on its second first-class target. Halting.");

    // With `--features interactive` the boot hands the machine to the serial line instead of
    // exiting. The gate builds without the feature, so its exit-code contract is untouched.
    #[cfg(feature = "interactive")]
    shellio::interactive();

    #[cfg(not(feature = "interactive"))]
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}
