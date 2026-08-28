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
//!   (the risk-advisor window overlaps 160+; a failure there is identified by its
//!    `[mlrisk] FAILED at risk-advisor invariant N: <name>` line, never by the code alone)
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
mod fwcfg;
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
mod vtd;

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

    // The IOMMU contract (ALET-P1-018, ADR-071): per-device translation spaces,
    // deny-by-default, the kernel image unmappable on both sides, named faults.
    // Modeled in software (the boot heap cannot afford sweep churn - ADR-063);
    // hardware realization is scoped in the gap register.
    kprintln!("");
    kprintln!("--- iommu selftests (device-visible memory enforced by translation) ---");
    match kernel_core::iommu::iommu_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[iommu] ALL {} IOMMU-CONTRACT INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[iommu] FAILED at iommu-contract invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(130 + idx as i32);
        }
    }

    // The power/performance contract (ALET-P2-022, ADR-076): frequency is authority, heat is a
    // hard ceiling. Elevation into the overclock band needs a live per-domain grant (attenuated
    // on delegation, clamping the domain back to nominal on revocation), the thermal envelope is
    // absolute, a trip clamps every domain and latches a cooldown, the governor never overclocks
    // and parks zero-demand domains, device power moves only along legal arcs, and every act
    // lands in the audit ledger. Modeled in software (the boot heap cannot afford sweep churn -
    // ADR-063); a hardware rung (MSR/CPPC programming) stays scoped in the gap register.
    kprintln!("");
    kprintln!("--- power/performance selftests (frequency is authority, heat is a ceiling) ---");
    match kernel_core::pm::pm_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[pm] ALL {} POWER-PERFORMANCE INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[pm] FAILED at power-performance invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(560 + idx as i32);
        }
    }

    // Lethe (REQ-ML-006, ADR-077): the resident performance advisor for the power/performance
    // contract - a frozen integer model (two decision trees, ALTH1) consulted by the advised
    // governor path. The suite proves the advisory discipline on the live path: with Lethe
    // present the overclock band stays authority-only and demanded silicon is never parked;
    // with the advisor absent or abstaining the advised path is bit-identical to the ADR-076
    // baseline governor; every way the blob can be wrong is a named refusal; and parity with
    // the trainer is a committed fixture replayed through the live observer.
    kprintln!("");
    kprintln!("--- lethe advisor selftests (the power governor learns) ---");
    match kernel_core::lethe::Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR) {
        Ok(a) => kprintln!(
            "[lethe] bundled advisor: {} freq nodes, {} idle nodes, worst case {} node visits per walk",
            a.shape().0,
            a.shape().1,
            a.shape().2
        ),
        Err(e) => kprintln!("[lethe] bundled advisor REFUSED: {:?}", e),
    }
    match kernel_core::lethe::lethe_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[lethe] ALL {} LETHE ADVISOR INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!("[lethe] FAILED at lethe invariant {}: {}", idx, name);
            ActiveHal::exit(580 + idx as i32);
        }
    }

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

    // The forest under LOAD, on this machine, with this machine's own clock: what an advice costs,
    // and what the advice actually changes about a schedule. Timings are REPORTED; only the
    // scale-invariant properties gate the boot (REQ-ML-002, ADR-056).
    match kernel_core::mlrisk::RiskAdvisor::load(kernel_core::mlrisk::BUNDLED_MODEL) {
        Ok(model) => {
            let advices = kernel_core::mlrisk_stress::BOOT_ADVICES;
            let tasks = kernel_core::mlrisk_stress::BOOT_TASKS;
            match kernel_core::mlrisk_stress::stress_suite(
                advices,
                tasks,
                |a, t| kernel_core::mlrisk_stress::measure::<ActiveHal>(&model, a, t),
                |a| {
                    kernel_core::mlrisk_stress::advice_stress::<ActiveHal>(
                        &model,
                        a,
                        0,
                        kernel_core::mlrisk_stress::HOT_SEED,
                    )
                },
                |n, passed, name| {
                    if passed {
                        kprintln!("  [pass {:>2}] {}", n, name);
                    } else {
                        kprintln!("  [FAIL {:>2}] {}", n, name);
                    }
                },
            ) {
                Ok((r, n)) => {
                    kprintln!(
                        "[mlrisk-stress] {} advices in {} ns => {} ps/advice, {} advices/s",
                        r.hot.advices,
                        r.hot.ns_total,
                        r.hot.ps_per_advice,
                        r.hot.per_second()
                    );
                    kprintln!(
                        "[mlrisk-stress] in-box census: {} low / {} elevated / {} abstain ({} from the conformal band)",
                        r.hot.low,
                        r.hot.elevated,
                        r.hot.abstain,
                        r.hot.band_abstain
                    );
                    kprintln!(
                        "[mlrisk-stress] out-of-box arrivals: {} of {} => {} abstain",
                        r.mixed.out_of_range,
                        r.mixed.advices,
                        r.mixed.abstain
                    );
                    kprintln!(
                        "[mlrisk-stress] schedule all-tied: {} tasks, {} decisive, {} positions move, plain {} ns vs advised {} ns",
                        r.tied.tasks,
                        r.tied.decisive,
                        r.tied.divergences,
                        r.tied.plain_ns,
                        r.tied.advised_ns
                    );
                    kprintln!(
                        "[mlrisk-stress] schedule 8 bands: {} tasks, {} decisive, {} positions move, plain {} ns vs advised {} ns",
                        r.banded.tasks,
                        r.banded.decisive,
                        r.banded.divergences,
                        r.banded.plain_ns,
                        r.banded.advised_ns
                    );
                    kprintln!(
                        "[mlrisk-stress] abstaining workload: {} tasks, {} positions move (must be 0)",
                        r.quiet.tasks,
                        r.quiet.divergences
                    );
                    kprintln!("[mlrisk-stress] ALL {} STRESS INVARIANTS HOLD", n);
                }
                Err((idx, name)) => {
                    kprintln!(
                        "[mlrisk-stress] FAILED at stress invariant {}: {}",
                        idx,
                        name
                    );
                    ActiveHal::exit(170 + idx as i32);
                }
            }
        }
        // The load-time refusal above already said which check refused the blob; a kernel with no
        // model has nothing to stress, and that is not a failure of this gate.
        Err(_) => kprintln!("[mlrisk-stress] SKIPPED: no verified model to stress"),
    }

    // The advisor takes up residence (REQ-ML-003, ADR-056). Everything above proved the *model*;
    // this proves the *machine consults it*. One verified forest is installed for the rest of this
    // boot, a commissioning workload is admitted through it — real feature derivation from live task
    // state, real margins, the real scheduler — and the counters it leaves behind are the ones
    // `mlstat` reports to a human at any later moment in the session.
    kprintln!("");
    kprintln!("--- resident risk advisor (the model the machine consults while it runs) ---");
    if kernel_core::mlsched::resident::install(
        kernel_core::mlrisk::BUNDLED_MODEL,
        kernel_core::mlsched::SUITE_MACHINE,
    ) {
        match kernel_core::mlsched::resident::shape() {
            Some((trees, nodes, compares)) => kprintln!(
                "[mlsched] RESIDENT: {} trees, {} nodes, worst case {} compares per advice",
                trees,
                nodes,
                compares
            ),
            None => kprintln!("[mlsched] RESIDENT: shape unavailable"),
        }
    } else {
        // Named absence, never a silent model-free boot that looks like an advised one.
        kprintln!(
            "[mlsched] NO RESIDENT MODEL: {:?}",
            kernel_core::mlsched::resident::model_error()
        );
    }
    match kernel_core::mlsched::mlsched_suite(|n, passed, name| {
        if passed {
            kprintln!("  [pass {:>2}] {}", n, name);
        } else {
            kprintln!("  [FAIL {:>2}] {}", n, name);
        }
    }) {
        Ok(n) => kprintln!("[mlsched] ALL {} LIVE-ADVISORY INVARIANTS HOLD", n),
        Err((idx, name)) => {
            kprintln!(
                "[mlsched] FAILED at live-advisory invariant {}: {}",
                idx,
                name
            );
            ActiveHal::exit(190 + idx as i32);
        }
    }
    {
        let c = kernel_core::mlsched::commission(4_096, 7);
        kprintln!(
            "[mlsched] commissioning: {} tasks admitted over {} s of machine time ({} cell bins)",
            c.admitted,
            c.span_secs,
            c.bins
        );
        kprintln!(
            "[mlsched] live census: {} advices — {} low / {} elevated / {} abstain ({} in band), {} out-of-box",
            c.stats.advices,
            c.stats.low,
            c.stats.elevated,
            c.stats.abstain,
            c.stats.band_abstain,
            c.stats.out_of_range
        );
        kprintln!(
            "[mlsched] watching: {} dispatches, {} finished / {} failed / {} evicted, {} ticks",
            c.stats.schedules,
            c.stats.finished,
            c.stats.failed,
            c.stats.evicted,
            c.stats.ticks
        );
        kprintln!(
            "[mlsched] continuity: span {} s, longest gap between advices {} s",
            c.stats.span_secs(),
            c.stats.max_gap_secs
        );
        // The claim that keeps this from being a demo: advice reordered equals and did nothing else.
        if !c.permutation {
            kprintln!(
                "[mlsched] FAILED: the advised drain was not a permutation of the model-free one"
            );
            ActiveHal::exit(189);
        }
        kprintln!(
            "[mlsched] advised drain is a permutation of the model-free one: {} tasks in, {} tasks out",
            c.admitted,
            c.admitted
        );
        kprintln!("[mlsched] the same numbers, as the console's `mlstat` renders them:");
        kernel_core::shell::report_risk_advisor(&mut |line: &str| kprintln!("  {}", line));
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

    // Every PCI device this boot will EVER drive comes up NOW - before the VT-d gate below turns
    // enforcement on - and stays live across it. That is how a real platform meets an IOMMU: the
    // hardware is already serving when translation flips on, so enforcement must prove it does not
    // disturb what is already running. (It also matches the emulator: QEMU's TCG virtqueues first
    // enabled WHILE GSTS.TES=1 mis-resolve their cached ring mappings - ADR-073 documents the
    // evidence. Bringing devices up quiet is the order firmware and OSes use anyway.)
    let mut blk_scratch = virtio::open_block(0);
    let mut blk_persist = virtio::open_block(1);
    let mut net_dev = virtio::network_device();
    let mut gpu_dev = virtio::graphics_device();

    // virtio-blk over PCI: the LAST first-class target to get real storage (REQ-DRV-005, ADR-037).
    // q35 has no virtio-mmio window, so `pci.rs` implements the shared driver's `Transport` seam over
    // the device's capability-described BAR regions. Skips green when no disk is attached; the boot
    // gate attaches a scratch disk and requires the marker. The suite ends by proving the whole
    // filesystem namespace over the real device.
    kprintln!("");
    kprintln!(
        "--- virtio-blk selftests (real PCI driver: discovery + virtqueue I/O + journal + filesystem) ---"
    );
    let virtio_result = match blk_scratch.as_mut() {
        None => {
            kprintln!("[virtio] no device (skipped)");
            Ok(0)
        }
        Some(dev) => virtio::block_suite(dev),
    };
    match virtio_result {
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

    // Long-running soak (ALET-P2-009, ADR-063): lifecycles under REPETITION — journal
    // transactions, namespace mutations, capability grants, task generations — on THIS machine's
    // own clock and heap meter. What gates is scale-free; what prints is this machine's truth:
    // throughput numbers are reported, never gated (QEMU-TCG nanoseconds are an emulator's), and
    // the heap line keeps the suite's own cost on this never-freeing heap a measured fact. The
    // journal phase's allocation-free claim is gated exactly where the meter can see it.
    kprintln!("");
    kprintln!("--- soak selftests (lifecycles under repetition: storage, grants, tasks) ---");
    {
        fn heap_meter() -> u64 {
            crate::heap::used_bytes() as u64
        }
        let before = heap_meter();
        match kernel_core::soak::soak_suite(
            kernel_core::soak::BOOT_LOAD,
            |load| {
                kernel_core::soak::campaign::<ActiveHal>(load, Some(&(heap_meter as fn() -> u64)))
            },
            |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            },
        ) {
            Ok((r, n)) => {
                kprintln!(
                    "[soak] journal: {} txs ({} verifies, {} recovers replayed) in {} ms => {} tx/s",
                    r.journal.txs,
                    r.journal.verifies,
                    r.journal.recovers_replayed,
                    r.journal.ns_total / 1_000_000,
                    r.journal.txs_per_second()
                );
                kprintln!(
                    "[soak] namespace: {} ops, every one audited => {} ops/s, {} survivors re-mounted",
                    r.fs.ops,
                    r.fs.ops_per_second(),
                    r.fs.final_survivors
                );
                kprintln!(
                    "[soak] grants: {} cycles, {}/{} unauthorized refused, {}/{} revoked accesses refused",
                    r.grants.cycles,
                    r.grants.unauthorized_refused,
                    r.grants.unauthorized_attempted,
                    r.grants.revoked_refused,
                    r.grants.revoked_attempted
                );
                kprintln!(
                    "[soak] tasks: {} generations, {} priority dispatches, each exactly-once",
                    r.tasks.generations,
                    r.tasks.priority_dispatched
                );
                kprintln!(
                    "[soak] heap: {} B used by the whole campaign (bump allocator never frees)",
                    heap_meter().saturating_sub(before)
                );
                kprintln!("[soak] ALL {} SOAK INVARIANTS HOLD", n);
            }
            Err((idx, name)) => {
                kprintln!("[soak] FAILED at soak invariant {}: {}", idx, name);
                ActiveHal::exit(400 + idx as i32);
            }
        }
    }

    // The machine measures itself (ALET-P2-010, ADR-064, REQ-PERF-002): the five load-bearing
    // paths — authority checks, capability-checked delivery, journal commits, scheduler
    // dispatches, console formatting — measured on THIS machine's own clock through the shared
    // Hal seam. Throughput is REPORTED, never gated (emulation timing is an emulator's); what
    // gates is everything structural: work really done, authority unbroken, commits read back,
    // steady state not first-touch setup, exactly-fair dispatch, byte-exact console arithmetic,
    // a rerun performing IDENTICAL work, and — the GUI half — the summary rendered GLYPH-EXACT
    // onto real framebuffer pages, wrap and scroll contracts included. The serial log above is
    // the TUI half of that claim.
    kprintln!("");
    kprintln!("--- benchmark selftests (this machine measures itself) ---");
    {
        let mut bench_pages: alloc::vec::Vec<usize> =
            alloc::vec::Vec::with_capacity(kernel_core::bench::FB_PAGES);
        for _ in 0..kernel_core::bench::FB_PAGES {
            match crate::frames::alloc_zeroed() {
                Some(f) => bench_pages.push(f.addr()),
                None => break,
            }
        }
        match kernel_core::bench::bench_suite::<ActiveHal>(
            kernel_core::bench::BOOT_LOAD,
            &bench_pages,
            |line| kprintln!("{}", line),
            |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            },
        ) {
            Ok((_report, n)) => {
                kprintln!("[bench] ALL {} BENCHMARK INVARIANTS HOLD", n);
                kprintln!("[bench] GUI half: the same numbers were proved ON THE FRAMEBUFFER");
            }
            Err((idx, name)) => {
                kprintln!("[bench] FAILED at benchmark invariant {}: {}", idx, name);
                ActiveHal::exit(420 + idx as i32);
            }
        }
    }

    // The cross-reboot claim, on REAL hardware (REQ-STOR-003, ADR-038). The persistent medium is the
    // SECOND disk: the scratch one above was reformatted by the destructive suites, this one is never
    // wiped. The boot gate boots twice against the same image file, so the second boot must FIND and
    // verify what the first wrote — the difference between "the OS can write" and "the OS remembers".
    kprintln!("");
    match blk_persist.as_mut() {
        Some(medium) => match kernel_core::persist::open_and_witness(medium) {
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

    // Custody crosses the platform boundary (ALET-P1-034, ADR-072). The vault root arrives over
    // the firmware configuration channel - NOT from the caller, and never from the disk it
    // protects. Absent or malformed delivery is a NAMED fact; the machine continues without the
    // vault rather than pretending custody happened.
    kprintln!("");
    match blk_persist.as_mut() {
        Some(medium) => {
            let mut bus = fwcfg::FwCfgIoports::new();
            let delivery = kernel_core::bootroot::deliver(&mut bus);
            kprintln!("[vault] {}", delivery.describe());
            if let kernel_core::bootroot::RootDelivery::Malformed(n) = &delivery {
                kprintln!(
                    "[vault] declared size: {} bytes (custody accepts exactly {})",
                    n,
                    kernel_core::bootroot::ROOT_LEN
                );
            }
            if matches!(delivery, kernel_core::bootroot::RootDelivery::Delivered(_)) {
                match kernel_core::bootroot::boot_suite(medium, &delivery, |n, passed, name| {
                    if passed {
                        kprintln!("  [pass {:>2}] {}", n, name);
                    } else {
                        kprintln!("  [FAIL {:>2}] {}", n, name);
                    }
                }) {
                    Ok(n) => kprintln!("[vault] ALL {} CUSTODY-DELIVERY INVARIANTS HOLD", n),
                    Err((idx, name)) => {
                        kprintln!(
                            "[vault] FAILED at custody-delivery invariant {}: {}",
                            idx,
                            name
                        );
                        ActiveHal::exit(460 + idx as i32);
                    }
                }
            }
        }
        None => kprintln!("[vault] no persistent medium attached (skipped)"),
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
        "--- network selftests (virtio-net over PCI: ARP-cache + ICMP echo + UDP-DHCP discovery against the gateway) ---"
    );
    // ADR-075 grant capture: the suite consumes the device, but its registry grants must reach
    // the VT-d gate. Init posted every buffer before this point and nothing later registers or
    // revokes, so a snapshot taken HERE is exactly what enforcement must allow.
    let mut net_windows: Option<alloc::vec::Vec<kernel_core::dma::Grant>> = None;
    match net_dev.take() {
        None => kprintln!("[net] no network device attached (skipped)"),
        Some(Err(e)) => {
            kprintln!("[net] device init FAILED: {:?}", e);
            ActiveHal::exit(220);
        }
        Some(Ok(net)) => {
            net_windows = Some(net.dma_grants());
            match kernel_core::virtionet::net_suite(net, |n, passed, name| {
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
            }
        }
    }

    // Graphics (REQ-GFX-001): the first real slice — a virtio-gpu function, the 2D resource
    // lifecycle, and a display-info round trip against hardware that ANSWERS. The suite ends by
    // asking the device to flush a resource it already destroyed, so the lifecycle proof is the
    // DEVICE's own error grammar, not our bookkeeping.
    kprintln!("");
    kprintln!(
        "--- graphics selftests (virtio-gpu over PCI: display info + 2D resource lifecycle) ---"
    );
    match gpu_dev.as_mut() {
        None => kprintln!("[gpu] no graphics device attached (skipped)"),
        Some(Err(e)) => {
            kprintln!("[gpu] device init FAILED: {:?}", e);
            ActiveHal::exit(300);
        }
        Some(Ok(gpu)) => {
            // One boot-log line of fact before the suite: what the machine says it will display.
            // SAFETY: the device is live and owned here; GET_DISPLAY_INFO is read-only.
            match unsafe { gpu.get_display_info() } {
                Ok(scans) => {
                    for (i, s) in scans.iter().enumerate().filter(|(_, s)| s.enabled) {
                        kprintln!("[gpu] display {}: {}x{}", i, s.rect.width, s.rect.height);
                    }
                }
                Err(e) => kprintln!("[gpu] display info error: {:?}", e),
            }
            match kernel_core::virtiogpu::gpu_suite(gpu, |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            }) {
                Ok(n) => kprintln!("[gpu] ALL {} VIRTIO-GPU INVARIANTS HOLD", n),
                Err((idx, name)) => {
                    kprintln!("[gpu] FAILED at gpu invariant {}: {}", idx, name);
                    ActiveHal::exit(301 + idx as i32);
                }
            }
            // The framebuffer console renders into REAL backing pages and hands the whole frame
            // to the display device — and proves DETACH revokes every page (REQ-GFX-002).
            match kernel_core::virtiogpu::console_suite(gpu, |n, passed, name| {
                if passed {
                    kprintln!("  [pass {:>2}] {}", n, name);
                } else {
                    kprintln!("  [FAIL {:>2}] {}", n, name);
                }
            }) {
                Ok(n) => kprintln!("[fbcon] ALL {} FRAMEBUFFER-CONSOLE INVARIANTS HOLD", n),
                Err((idx, name)) => {
                    kprintln!("[fbcon] FAILED at fbconsole invariant {}: {}", idx, name);
                    ActiveHal::exit(341 + idx as i32);
                }
            }
        }
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

    // The IOMMU contract meets hardware (ALET-P1-018 third rung, ADR-071/073/075): the VT-d unit
    // QEMU emulates on q35 is discovered through the ACPI DMAR table, then programmed with a
    // PER-DEVICE WINDOW DOMAIN per driven function - exactly the frames each driver's own DMA
    // registry vouches for, nothing else - plus context entries for those functions ONLY, and
    // enforcement turned ON. The LIVE block function (serving since before the flip) is probed:
    // it walks clean under its windows; a revoked PAGE is denied with a fault naming its
    // source-id and address; restoring the leaf returns silence; enforcement stays latched.
    //
    // This is deliberately the LAST gate: every DMA-dependent suite above ran while the machine
    // was still quiet, which is both how real firmware/OSes meet an IOMMU and the only ordering
    // QEMU's TCG supports (its per-device ring caches mis-resolve once translation is on mid-run
    // - 'bogus descriptor or out of resources' - masking completions but not the unit's verdicts;
    // ADR-073 documents the evidence trail). Graceful absence (no DMAR declared - VirtualBox)
    // skips green; a PRESENT unit that fails any invariant exits 500+i.
    kprintln!("");
    kprintln!("--- vt-d selftests (per-device windows programmed into a real remapping unit) ---");
    // The grant table: every DRIVEN function contributes what ITS registry vouches for. Block
    // devices are alive right here; the GPU console too; the network device was consumed by its
    // suite, which captured its grants before the move (identical - idle queues never change).
    let mut dma_grants: alloc::vec::Vec<vtd::DeviceGrant> = alloc::vec::Vec::new();
    if let (Some(b), Some(d)) = (unsafe { pci::find_virtio_blk() }, blk_scratch.as_ref()) {
        dma_grants.push(vtd::DeviceGrant::new(b, d.dma_grants()));
    }
    if let (Some(b), Some(d)) = (unsafe { pci::find_virtio_blk_nth(1) }, blk_persist.as_ref()) {
        dma_grants.push(vtd::DeviceGrant::new(b, d.dma_grants()));
    }
    if let (Some(b), Some(w)) = (unsafe { pci::find_virtio_net_nth(0) }, net_windows.as_ref()) {
        dma_grants.push(vtd::DeviceGrant::from_grants(b, w));
    }
    if let (Some(b), Some(Ok(g))) = (unsafe { pci::find_virtio_gpu_nth(0) }, gpu_dev.as_ref()) {
        dma_grants.push(vtd::DeviceGrant::new(b, g.dma_grants()));
    }
    match vtd::dmar_suite(&dma_grants, blk_scratch.as_mut(), blk_persist.as_mut()) {
        Ok(_) => {}
        Err((idx, _name)) => {
            // The suite already printed "[dmar] FAILED at vt-d invariant N: <detail>" - the NAME
            // is the diagnosis; this window is only the coarse index.
            ActiveHal::exit(500 + idx as i32);
        }
    }

    kprintln!("");
    kprintln!("[e2e] PASS — x86-64 UEFI boot + arch init + timer IRQ + memory-management + virtual-memory + 13 spine invariants + capability-lifetime + SMP + ring-3 user-mode + filesystem + console");
    kprintln!("[e2e] Aletheia booted as its own OS on AMD64. Halting.");

    // With `--features interactive` the boot hands the machine to the serial line instead of
    // exiting. The gate builds without the feature, so its exit-code contract is untouched.
    #[cfg(feature = "interactive")]
    shellio::interactive(blk_persist.take());

    #[cfg(not(feature = "interactive"))]
    ActiveHal::exit(0)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("[KERNEL PANIC] {}", info);
    exit::exit(101)
}
