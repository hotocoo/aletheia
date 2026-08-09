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
mod conirq;
mod frames;
mod hal;
mod heap;
mod semihosting;
mod shellio;
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

    // Physical memory: bring up the frame allocator over the RAM above the static kernel region,
    // with its ownership table attached (REQ-MM-002). A pool whose tail has no ownership state
    // could not detect a double free there, so failing to attach is fatal, not a warning.
    if !frames::init() {
        kprintln!("[mm] FATAL: frame ownership table does not cover the pool");
        semihosting::exit(39);
    }
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

    // What a scancode MEANS (REQ-CON-003, ADR-049). This target has no PS/2 controller — the QEMU
    // `virt` machine exposes none, an honest architectural difference — but the decoder is
    // arch-independent and its output alphabet is the console's shared contract, so it is proved
    // here too rather than only where the hardware happens to be.
    kprintln!("");
    kprintln!("--- keyboard-decode selftests (scancodes to the bytes the console accepts) ---");
    match kernel_core::keymap::keymap_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[keys] ALL {} KEYBOARD-DECODE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[keys] FAILED at keyboard-decode invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(90 + idx as i32);
        }
    }

    // Capability lifetime across a reboot (REQ-CAP-008, ADR-048). The spine suite above proves
    // authority is correct while the machine is up; this proves what survives a restart and — far
    // more important — what a restored registry must REFUSE, since a persisted registry is
    // untrusted input and a load that trusts it is a minting path with no delegation behind it.
    kprintln!("");
    kprintln!("--- capability-lifetime selftests (a persisted registry is untrusted input) ---");
    match kernel_core::capstore::capstore_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[cap] ALL {} CAPABILITY-LIFETIME INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[cap] FAILED at capability-lifetime invariant {}: {}",
                idx,
                name
            );
            semihosting::exit(110 + idx as i32);
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
            semihosting::exit(240 + idx as i32);
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

    // Filesystem: the named-object namespace over the journaled block store (REQ-FS-001, ADR-035).
    // The namespace is arch-independent, so every target proves the SAME behaviors over a RAM-disk
    // device — and on this target `virtio::selftest` above additionally proves them over the REAL
    // virtio-blk driver, which is what makes the crash-atomicity claim a hardware claim.
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
            semihosting::exit(160 + idx as i32);
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
            semihosting::exit(200 + idx as i32);
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
                semihosting::exit(210);
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
            semihosting::exit(220);
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
                semihosting::exit(220 + idx as i32);
            }
        },
    }

    // The interactive console (REQ-CON-001, ADR-044). Every gate above ends in a verdict and an
    // exit; this is the subsystem that lets the machine STAY up and answer a human. It is proved
    // here the same way everything else is — a scripted session against a real namespace — so the
    // gate covers the code an interactive boot runs, not a separate path that only humans see.
    kprintln!("");
    kprintln!("--- input-ring selftests (what an interrupt hands the shell) ---");
    match shellio::ring_selftest() {
        Ok(n) => kprintln!("[conring] ALL {} INPUT-RING INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[conring] FAILED at input-ring invariant {}: {}", idx, name);
            semihosting::exit(230 + idx as i32);
        }
    }

    kprintln!("--- console selftests (line editing + command dispatch over the namespace) ---");
    match shellio::selftest() {
        Ok(n) => kprintln!("[console] ALL {} CONSOLE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[console] FAILED at console invariant {}: {}", idx, name);
            semihosting::exit(250 + idx as i32);
        }
    }

    bench::run();

    kprintln!("");
    kprintln!(
        "[e2e] PASS — boot + spine + {} invariants + capability-lifetime + memory-management + virtual-memory + user-mode + filesystem + console + benchmark complete",
        13
    );

    // Built with `--features interactive`, the boot does NOT end here: it hands the machine to
    // whoever is at the serial line. The gate builds without the feature, so its exit-code contract
    // is untouched (a session has no verdict to give).
    #[cfg(feature = "interactive")]
    shellio::interactive();

    #[cfg(not(feature = "interactive"))]
    semihosting::exit(0);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    semihosting::exit(101);
}
