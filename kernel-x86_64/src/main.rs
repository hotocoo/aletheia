//! Aletheia microkernel — bare-metal AMD64/x86-64, UEFI boot (ADR-019 first-class target).
//!
//! Boot flow (outside-in): firmware `#[entry]` -> capture GOP framebuffer + PE image bounds ->
//! **ExitBootServices** (Aletheia takes the machine) -> own GDT + IDT -> PIC remap + PIT timer ->
//! `sti` and PROVE a timer IRQ fires -> own frame allocator -> **build the kernel's OWN address map
//! and make CR3 point at it** (`kmap`, ALET-P1-031: the firmware's W+X tree stops translating) ->
//! re-prove the capability-secure spine invariants in kernel space -> SMP -> ring 3 -> exit.
//!
//! "Aletheia boots as its own OS" is honest here precisely because it calls ExitBootServices and
//! then runs on its OWN interrupt/timer/segment state — the UEFI firmware is the hardware/platform
//! integration layer (ADR-019), not an OS underneath us. The QEMU exit code + serial log are the
//! machine-checkable verdict; the GOP framebuffer is the human-visible one (VMware shows this):
//!   exit 33  => all invariants held (e2e PASS)   [isa-debug-exit encodes success 0 as 0x10]
//!   exit 0x10+i (i=10+idx) => spine invariant idx failed
//!   i = 30+idx memory · 40+idx virtual-memory · 60+idx SMP · 80+idx ring-3 · 150+idx risk-advisor
//!   28 => the LIVE address space violates W^X · 29 => no owned frame pool (both fail-closed)
//!   101 => panic, 102 => double fault, 103 => #GP, 104 => #PF, 105 => #UD

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

#[macro_use]
mod console;
mod acpi;
mod cell;
mod conirq;
mod exit;
mod framebuffer;
mod frames;
mod gdt;
mod hal;
mod heap;
mod idt;
mod kmap;
mod pci;
mod pic;
mod pit;
mod ps2;
mod serial;
mod shellio;
mod smp;
mod usermode;
mod virtio;
mod vm;

// Shared, arch-independent Aletheia spine + invariant suite — now a real `kernel-core` dependency
// (defined once there, not `#[path]`-copied per target; gap-register Issue 1). This target proves
// the SAME invariants the aarch64 and RISC-V kernels do, from the SAME source.
use kernel_core::{selftest, spine};

use uefi::boot;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::proto::loaded_image::LoadedImage;

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

    // Stash the ACPI RSDP while the config table is still reachable: the SMP suite discovers the
    // application processors from the MADT after ExitBootServices (ACPI-reclaim memory persists).
    uefi::system::with_config_table(|entries| {
        for e in entries {
            if e.guid == uefi::table::cfg::ConfigTableEntry::ACPI2_GUID {
                acpi::stash_rsdp(e.address as usize);
            }
        }
    });

    // Where the firmware loaded us. A PE image has no `linker.ld` symbols, so this — plus the
    // image's own section table — is how the kernel learns its text/rodata/data bounds and can
    // build its OWN W^X-correct address map instead of inheriting OVMF's (ALET-P1-031, REQ-MM-006).
    // Must happen while boot services live; the image memory itself survives ExitBootServices.
    capture_image_bounds();

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

/// Record the loaded image's base + size from `LoadedImage`, and log it. A failure here is not
/// fatal — it makes `kmap::build` refuse, which the boot reports — because the kernel still runs
/// (on the firmware's map) exactly as it did before this map existed.
fn capture_image_bounds() {
    match boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()) {
        Ok(image) => {
            let (base, size) = image.info();
            kmap::capture_image(base as usize, size as usize);
            kprintln!(
                "[uefi] loaded image: base={:#x} size={:#x} (PE section table = our W^X bounds)",
                base as usize,
                size
            );
        }
        Err(e) => kprintln!(
            "[uefi] WARNING: LoadedImage unavailable ({:?}) — the kernel cannot build its own map",
            e.status()
        ),
    }
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
    // W^X needs EFER.NXE before any page is mapped (REQ-MM-006, ADR-034); firmware does not
    // guarantee it, so we enable it ourselves and report what the CPU allows.
    match vm::enable_exec_protections() {
        (true, true) => kprintln!(
            "[mm] EFER.NXE + CR4.SMEP enabled — W^X enforceable by paging, user pages not ring-0 executable"
        ),
        (nx, smep) => kprintln!(
            "[mm] WARNING: exec protections incomplete (NX={}, SMEP={}) — W^X degraded on this CPU",
            nx,
            smep
        ),
    }

    let (fbase, fcount) = frames::init_from_uefi(memory_map);
    // Fail-closed (REQ-MM-002): no conventional RAM, or an ownership table that could not cover the
    // pool, both surface as a zero-frame pool. Running on would mean allocating frames nothing owns.
    if fcount == 0 {
        kprintln!(
            "[mm] FATAL: no owned frame pool (no conventional RAM or ownership table too small)"
        );
        ActiveHal::exit(29);
    }
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

    // --- the kernel's OWN address map (ALET-P1-031, REQ-MM-006, ADR-034) ---
    // Built from the PE image bounds, audited below by the vm gate, and NOT activated in this
    // commit: CR3 still points at OVMF's tree. Building before activating means a wrong map fails
    // an invariant instead of triple-faulting the machine.
    match kmap::build(memory_map) {
        Some((root, r)) => kprintln!(
            "[mm] kernel map built @ {:#x}: {} MiB identity, {} huge + {} page leaves, {} image-split blocks, {} table frames",
            root,
            r.covered / (1024 * 1024),
            r.huge_leaves,
            r.page_leaves,
            r.split_blocks,
            r.tables
        ),
        None => kprintln!(
            "[mm] WARNING: kernel map NOT built (no image bounds / unparsable PE sections / no frames)"
        ),
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

    // --- take the machine's address space too (ALET-P1-031) ---
    // The vm gate above proved the map is complete and W^X-correct. Only now does CR3 move: every
    // check after this line — the spine invariants, SMP bring-up, the whole ring-3 suite — runs on
    // the kernel's OWN tree, which is the only way to claim it is usable rather than merely built.
    if kmap::activate() {
        let live = vm::audit_all(vm::active_root());
        kprintln!(
            "[mm] kernel map ACTIVE (CR3 = {:#x}) — the firmware's tree is no longer translating",
            vm::active_root()
        );
        kprintln!(
            "[mm] live W^X audit: {} leaves, {} violations",
            live.leaves,
            live.dynamic_violations + live.bootstrap_violations
        );
        if live.dynamic_violations + live.bootstrap_violations != 0 {
            kprintln!("[mm] FATAL: the live address space violates W^X");
            ActiveHal::exit(28);
        }
    } else {
        kprintln!("[mm] WARNING: kernel map not activated — still translating through OVMF's tree");
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

    // What a scancode MEANS (REQ-CON-003, ADR-049). Arch-independent, so it is proved on every
    // target even though only this one has the hardware — the console's byte alphabet is a shared
    // contract, and a decoder that could emit outside it would be a way to feed the line editor a
    // byte it has no rule for from a device someone else is holding.
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
            ActiveHal::exit(90 + idx as i32);
        }
    }

    // The keyboard itself (REQ-CON-003, ADR-049). Run on EVERY boot, not only interactive ones: a
    // driver that only runs when someone is sitting at the machine is a driver no gate covers.
    kprintln!("");
    kprintln!(
        "--- PS/2 keyboard bring-up (ACPI declaration, controller + port + device self-test) ---"
    );
    match ps2::keyboard_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[ps2] ALL {} KEYBOARD INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[ps2] FAILED at keyboard invariant {}: {}", idx, name);
            ActiveHal::exit(130 + idx as i32);
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
            ActiveHal::exit(110 + idx as i32);
        }
    }

    // The frozen risk forest this image carries (REQ-ML-001, ADR-056). It is ADVISORY: it may
    // reorder tasks whose effective priority is already equal, and nothing else. So the suite proves
    // the advisory property itself — an abstaining model schedules bit-identically to the model-free
    // kernel, and priority is never traded for risk — alongside exact parity with the trainer and a
    // NAMED refusal for every way a blob can be wrong. The printed invariant NAME is the
    // authoritative diagnosis; the exit code is a coarse index on top of it.
    kprintln!("");
    kprintln!("--- risk-advisor selftests (a frozen integer forest, advisory only) ---");
    match kernel_core::mlrisk::RiskAdvisor::load(kernel_core::mlrisk::BUNDLED_MODEL) {
        Ok(m) => kprintln!(
            "[mlrisk] bundled forest: {} trees, {} nodes, worst case {} compares per advice",
            m.trees(),
            m.nodes(),
            m.worst_case_compares()
        ),
        // Absence is NAMED, never silent: the kernel says which check refused the blob and carries
        // on with its deterministic policy (the suite below then fails on this same load).
        Err(e) => kprintln!("[mlrisk] bundled forest REFUSED: {:?}", e),
    }
    match kernel_core::mlrisk::mlrisk_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[mlrisk] ALL {} RISK-ADVISOR INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[mlrisk] FAILED at risk-advisor invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(150 + idx as i32);
        }
    }

    // SMP: discover the APs from the ACPI MADT, wake them with INIT-SIPI-SIPI through the
    // real-mode trampoline, and prove the 13-invariant cross-core substrate (REQ-SMP-002 parity
    // with aarch64/RISC-V). MUST run before the ring-3 suite: that suite repoints IRQ0 at its own
    // context-switching entry and leaves IF=0, which would strand the PIT deadline clock used here.
    kprintln!("");
    kprintln!(
        "--- SMP selftests (MADT + INIT-SIPI-SIPI bring-up + cross-core concurrency substrate) ---"
    );
    match smp::selftest() {
        Ok(0) => {}
        Ok(n) => kprintln!("[smp] ALL {} SMP INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[smp] FAILED at SMP invariant {}: {}", idx, name);
            ActiveHal::exit(60 + idx as i32);
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

    // virtio-blk over PCI: the LAST first-class target to get real storage (REQ-DRV-005, ADR-037).
    // q35 has no virtio-mmio window, so `pci.rs` implements the shared driver's `Transport` seam over
    // the device's capability-described BAR regions. Skips green when no disk is attached; the boot
    // gate attaches a scratch disk and requires the marker. The suite ends by proving the whole
    // filesystem namespace over the real device.
    kprintln!("");
    kprintln!(
        "--- virtio-blk selftests (real PCI driver: discovery + virtqueue I/O + journal + filesystem) ---"
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
    // device (this target has no block driver yet — that is REQ-DRV-001 on x86-64, not a namespace
    // difference).
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

    // Networking (REQ-NET-001/002, ADR-041) — same shared driver and stack as the other targets, over the
    // PCI transport. ARP the gateway, then ping it: a transmit-only driver would prove nothing.
    kprintln!("");
    kprintln!(
        "--- network selftests (virtio-net over PCI: ARP + ICMP echo against the gateway) ---"
    );
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
    kprintln!("[e2e] PASS — x86-64 UEFI boot + arch init + timer IRQ + memory-management + virtual-memory + 13 spine invariants + capability-lifetime + SMP + ring-3 user-mode + filesystem + console");
    kprintln!("[e2e] Aletheia booted as its own OS on AMD64. Halting.");

    // With `--features interactive` the boot hands the machine to the serial line instead of
    // exiting. The gate builds without the feature, so its exit-code contract is untouched.
    #[cfg(feature = "interactive")]
    shellio::interactive();

    #[cfg(not(feature = "interactive"))]
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}
