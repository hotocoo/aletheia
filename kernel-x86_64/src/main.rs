//! Aletheia microkernel — bare-metal AMD64/x86-64, UEFI boot (ADR-019 first-class target).
//!
//! Boot flow (outside-in): firmware `#[entry]` -> capture GOP framebuffer -> **ExitBootServices**
//! (Aletheia takes the machine) -> own GDT + IDT -> PIC remap + PIT timer -> `sti` and PROVE a
//! timer IRQ fires -> re-prove the capability-secure spine invariants in kernel space -> exit.
//!
//! "Aletheia boots as its own OS" is honest here precisely because it calls ExitBootServices and
//! then runs on its OWN interrupt/timer/segment state — the UEFI firmware is the hardware/platform
//! integration layer (ADR-019), not an OS underneath us. The QEMU exit code + serial log are the
//! machine-checkable verdict; the GOP framebuffer is the human-visible one (VMware shows this):
//!   exit 33  => all invariants held (e2e PASS)   [isa-debug-exit encodes success 0 as 0x10]
//!   exit 0x10+i (i=10+idx) => spine invariant idx failed
//!   101 => panic, 102 => double fault, 103 => #GP, 104 => #PF, 105 => #UD

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

#[macro_use]
mod console;
mod cell;
mod exit;
mod framebuffer;
mod frames;
mod gdt;
mod hal;
mod heap;
mod idt;
mod pic;
mod pit;
mod serial;
mod usermode;
mod vm;

// Shared, arch-independent Aletheia spine + invariant suite — now a real `kernel-core` dependency
// (defined once there, not `#[path]`-copied per target; gap-register Issue 1). This target proves
// the SAME invariants the aarch64 and RISC-V kernels do, from the SAME source.
use kernel_core::{selftest, spine};

use uefi::boot;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

struct FbInfo {
    base: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    bgr: bool,
}

#[entry]
fn efi_main() -> Status {
    // Serial works immediately (direct port I/O), so we log from the very first instruction.
    serial::init();
    kprintln!("");
    kprintln!("========================================");
    kprintln!(" Aletheia — AI-native OS   (x86-64 / AMD64 bring-up)");
    kprintln!("========================================");
    kprintln!("[uefi] firmware handoff stage; locating GOP framebuffer...");

    let fb = capture_framebuffer();

    kprintln!("[uefi] calling ExitBootServices — Aletheia takes ownership of the machine");
    // SAFETY: the only boot-services borrows (GOP ScopedProtocol + FrameBuffer) were dropped inside
    // `capture_framebuffer`; no reference into boot-services memory survives this call.
    let memory_map = unsafe { boot::exit_boot_services(None) };

    if let Some(info) = fb {
        // SAFETY: the GOP framebuffer is identity-mapped MMIO, exclusively ours now firmware exited.
        let f = unsafe {
            framebuffer::FrameBuffer::new(info.base, info.width, info.height, info.stride, info.bgr)
        };
        console::set_framebuffer(f);
    }

    kmain(&memory_map)
}

/// Capture framebuffer geometry while boot services are still alive; the raw base pointer stays
/// valid after exit (identity-mapped MMIO). All protocol borrows are dropped before returning.
fn capture_framebuffer() -> Option<FbInfo> {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).ok()?;
    let mode = gop.current_mode_info();
    let (width, height) = mode.resolution();
    let stride = mode.stride();
    let format = mode.pixel_format();
    let bgr = match format {
        PixelFormat::Bgr => true,
        PixelFormat::Rgb => false,
        other => {
            kprintln!(
                "[gop] pixel format {:?} has no linear framebuffer; serial-only",
                other
            );
            return None;
        }
    };
    let mut buffer = gop.frame_buffer();
    let base = buffer.as_mut_ptr();
    let size = buffer.size();
    kprintln!(
        "[gop] {}x{} stride={} fmt={:?} base={:p} size={:#x}",
        width,
        height,
        stride,
        format,
        base,
        size
    );
    Some(FbInfo {
        base,
        width,
        height,
        stride,
        bgr,
    })
}

fn summarize_memory(map: &MemoryMapOwned) -> (usize, u64) {
    let mut entries = 0usize;
    let mut conventional = 0u64;
    for d in map.entries() {
        entries += 1;
        if d.ty == MemoryType::CONVENTIONAL {
            conventional += d.page_count * 4096;
        }
    }
    (entries, conventional)
}

fn kmain(memory_map: &MemoryMapOwned) -> ! {
    use hal::{ActiveHal, Hal};

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

    gdt::init();
    kprintln!("[boot] GDT loaded (flat 64-bit code/data); segment registers reloaded");
    idt::init();
    kprintln!("[boot] IDT loaded (CPU exception vectors + IRQ0 timer)");
    pic::init();
    kprintln!("[boot] 8259 PIC remapped to 0x20..0x2F; IRQ0 unmasked");
    pit::init();
    kprintln!("[boot] 8254 PIT programmed to {} Hz", pit::FREQ_HZ);

    let (entries, conventional) = summarize_memory(memory_map);
    kprintln!(
        "[boot] memory: {} UEFI map entries, {} MiB usable conventional RAM",
        entries,
        conventional / (1024 * 1024)
    );
    kprintln!(
        "[boot] heap: 8 MiB static region; {} B used after init",
        heap::used_bytes()
    );
    kprintln!(
        "[boot] privilege: CPL {} (ring 0 = kernel)",
        ActiveHal::current_privilege()
    );

    x86_64::instructions::interrupts::enable();
    kprintln!("[boot] interrupts enabled (sti); waiting for timer IRQs...");
    let target = 5u64;
    while pit::ticks() < target {
        x86_64::instructions::hlt();
    }
    kprintln!(
        "[timer] OK: {} ticks via IRQ0 — interrupts + timer are LIVE",
        pit::ticks()
    );
    kprintln!("[hal] rdtsc monotonic sample: {}", ActiveHal::timer_ticks());

    // --- physical memory management (P5): take ownership of the RAM the firmware handed us ---
    let (fbase, fcount) = frames::init_from_uefi(memory_map);
    kprintln!(
        "[mm] frame allocator: {} frames ({} MiB) from the largest conventional region @ {:#x}",
        fcount,
        fcount * frames::FRAME_SIZE / (1024 * 1024),
        fbase
    );
    kprintln!("");
    kprintln!("--- memory-management selftests (physical frames, from the UEFI map) ---");
    match frames::selftest() {
        Ok(n) => kprintln!("[mm] ALL {} MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[mm] FAILED at memory invariant {}: {}", idx, name);
            ActiveHal::exit(30 + idx as i32);
        }
    }

    // --- virtual memory (P5): walk + edit the live UEFI page-table hierarchy we now own ---
    kprintln!("");
    kprintln!("--- virtual-memory selftests (MMU: map/unmap over the live UEFI hierarchy) ---");
    match vm::selftest() {
        Ok(n) => kprintln!("[vm] ALL {} VIRTUAL-MEMORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[vm] FAILED at virtual-memory invariant {}: {}", idx, name);
            ActiveHal::exit(40 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!("--- invariant selftests (M1 acceptance, re-proved in x86-64 kernel space) ---");
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

    // Ring-3 user-mode: drop to unprivileged ring 3 and prove the capability-gated syscall boundary,
    // hardware address-space isolation, per-process PML4 spaces, and PIT-driven preemptive
    // multitasking (the x86-64 twin of the aarch64 EL0 suite). Masks interrupts for its duration.
    kprintln!("");
    kprintln!("--- user-mode selftests (ring-3 privilege boundary: cap-gated syscall + isolation + preemption) ---");
    match usermode::selftest() {
        Ok(n) => {
            kprintln!("[usermode] ALL {} RING-3 BOUNDARY INVARIANTS HOLD", n);
            // Keep IF=0 through the halt/exit (as aarch64/RISC-V do). Re-enabling here would let a
            // PIT IRQ latched during the ring-3 suite fire between "[e2e] PASS" and exit(0) and, with
            // no live scheduler left, resume_return would jump into the last excursion's now-stale
            // KERNEL_CTX — a triple fault surfacing as QEMU exit 255. Nothing below needs interrupts.
        }
        Err((idx, name)) => {
            kprintln!("[usermode] FAILED at ring-3 invariant {}: {}", idx, name);
            ActiveHal::exit(80 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!("[e2e] PASS — x86-64 UEFI boot + arch init + timer IRQ + memory-management + virtual-memory + 11 spine invariants + ring-3 user-mode");
    kprintln!("[e2e] Aletheia booted as its own OS on AMD64. Halting.");
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}
